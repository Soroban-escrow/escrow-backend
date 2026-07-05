//! `SorobanChain`: the production implementation of the chain ports, over Soroban RPC.

use crate::{
    error::ChainError,
    events::{RawEvent, cursor_ledger},
    keys,
    ports::{ChainReader, EventSource, TxBuilder},
    rpc::{EventsWire, GetTxWire, LatestLedgerWire, LedgerEntriesWire, RpcClient, SendWire, SimulateWire, SimulationOk, describe_tx_error},
    scval, tx,
    types::{ChainBounty, ContractConfig, EventsPage, FundParams, TxOutcome},
};
use async_trait::async_trait;
use escrow_core::BountyStatus;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use stellar_xdr::{
    AccountId, LedgerEntryData, LedgerKey, LedgerKeyAccount, Limits, PublicKey, ReadXdr, ScVal, Transaction, Uint256, WriteXdr,
};

#[derive(Debug, Clone)]
pub struct ChainSettings {
    pub rpc_url: String,
    pub network_passphrase: String,
    pub contract_id: String,
    pub timeout: Duration,
}

impl ChainSettings {
    pub fn new(rpc_url: &str, network_passphrase: &str, contract_id: &str) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            network_passphrase: network_passphrase.into(),
            contract_id: contract_id.into(),
            timeout: Duration::from_secs(20),
        }
    }
}

/// Cheap to clone; shares one HTTP client.
#[derive(Clone)]
pub struct SorobanChain {
    pub(crate) rpc: RpcClient,
    pub(crate) network_id: [u8; 32],
    pub(crate) contract: [u8; 32],
    pub(crate) contract_id: Arc<str>,
}

/// How long a transaction we prepare for a person's wallet stays valid: time to open the wallet and approve.
const USER_TX_VALID_SECS: u64 = 600;
/// The relayer submits immediately; a short window means an unlucky transaction expires instead of lingering.
pub(crate) const RELAYER_TX_VALID_SECS: u64 = 120;
/// Source for read-only simulations: the all-zero account, which needs no funds or sequence number.
const READ_SOURCE: [u8; 32] = [0; 32];

impl SorobanChain {
    pub fn new(s: &ChainSettings) -> Result<Self, ChainError> {
        Ok(Self {
            rpc: RpcClient::new(&s.rpc_url, s.timeout)?,
            network_id: tx::network_id(&s.network_passphrase),
            contract: keys::contract_bytes(&s.contract_id)?,
            contract_id: s.contract_id.as_str().into(),
        })
    }

    pub fn contract_id(&self) -> &str {
        &self.contract_id
    }

    pub(crate) fn network_id(&self) -> [u8; 32] {
        self.network_id
    }

    pub(crate) async fn simulate(&self, tx: &Transaction) -> Result<SimulationOk, ChainError> {
        let xdr = tx::to_b64(&tx::envelope(tx.clone()))?;
        let wire: SimulateWire = self.rpc.call("simulateTransaction", json!({ "transaction": xdr })).await?;
        wire.into_ok()
    }

    /// Account sequence number and XLM balance (in stroops), or `Invalid` when the account does not exist.
    pub(crate) async fn account(&self, address: &str) -> Result<(i64, i64), ChainError> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(keys::account_bytes(address)?))),
        });
        let wire: LedgerEntriesWire = self.rpc.call("getLedgerEntries", json!({ "keys": [key.to_xdr_base64(Limits::none())?] })).await?;
        let entry = wire
            .entries
            .and_then(|e| e.into_iter().next())
            .ok_or_else(|| ChainError::Invalid(format!("account {} does not exist on this network; fund it first", short(address))))?;
        match LedgerEntryData::from_xdr_base64(&entry.xdr, Limits::none())? {
            LedgerEntryData::Account(a) => Ok((a.seq_num.0, a.balance)),
            _ => Err(ChainError::Decode("expected an account entry".into())),
        }
    }

    async fn read(&self, function: &str, args: Vec<ScVal>) -> Result<ScVal, ChainError> {
        let t = tx::invoke_tx(READ_SOURCE, 0, self.contract, function, args, 0)?;
        self.simulate(&t).await?.result.ok_or_else(|| ChainError::Decode(format!("{function} returned nothing")))
    }

    /// Builds, simulates and assembles a call for `source` to sign. Returns the unsigned envelope as base64.
    async fn prepare(&self, source: &str, function: &str, args: Vec<ScVal>) -> Result<String, ChainError> {
        let (seq, _) = self.account(source).await?;
        let t = tx::invoke_tx(keys::account_bytes(source)?, seq + 1, self.contract, function, args, tx::now_secs() + USER_TX_VALID_SECS)?;
        let sim = self.simulate(&t).await?;
        tx::to_b64(&tx::envelope(tx::assemble(t, &sim)?))
    }

    /// Submits a signed envelope; returns the hash. `PENDING` and `DUPLICATE` both mean "it is on its way".
    pub(crate) async fn send(&self, xdr_b64: &str) -> Result<(), ChainError> {
        let w: SendWire = self.rpc.call("sendTransaction", json!({ "transaction": xdr_b64 })).await?;
        match w.status.as_str() {
            "PENDING" | "DUPLICATE" => Ok(()),
            "TRY_AGAIN_LATER" => Err(ChainError::Busy),
            _ => Err(ChainError::Rejected(describe_tx_error(w.error_result_xdr.as_deref()))),
        }
    }

    /// Polls until the transaction is final. `NotFound` after the window means it never showed up.
    pub async fn wait_for_tx(&self, hash: &str, window: Duration) -> Result<TxOutcome, ChainError> {
        let start = std::time::Instant::now();
        let mut delay = Duration::from_millis(500);
        loop {
            let w: GetTxWire = self.rpc.call("getTransaction", json!({ "hash": hash })).await?;
            match w.status.as_str() {
                "SUCCESS" => return Ok(TxOutcome::Success),
                "FAILED" => return Ok(TxOutcome::Failed),
                _ => {}
            }
            if start.elapsed() >= window {
                return Ok(TxOutcome::NotFound);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 3 / 2).min(Duration::from_secs(4));
        }
    }
}

fn short(a: &str) -> String {
    format!("{}…{}", &a[..a.len().min(6)], &a[a.len().saturating_sub(4)..])
}

pub fn parse_bounty(v: &ScVal) -> Result<ChainBounty, ChainError> {
    let f = scval::Fields::of(v)?;
    let status_name = f.variant("status")?;
    Ok(ChainBounty {
        id: f.u64("id")?,
        maintainer: f.address("maintainer")?,
        repo_id: f.u64("repo_id")?,
        issue: f.u64("issue")?,
        token: f.address("token")?,
        amount: f.i128("amount")?,
        fee_bps: f.u32("fee_bps")?,
        deadline: f.u64("deadline")?,
        relayer_allowed: f.bool("relayer_allowed")?,
        status: BountyStatus::from_contract_name(&status_name)
            .ok_or_else(|| ChainError::Decode(format!("unknown status {status_name}")))?,
        winner: f.opt_address("winner")?,
        created_at: f.u64("created_at")?,
    })
}

pub fn parse_config(v: &ScVal) -> Result<ContractConfig, ChainError> {
    let f = scval::Fields::of(v)?;
    Ok(ContractConfig {
        admin: f.address("admin")?,
        relayer: f.address("relayer")?,
        arbiter: f.address("arbiter")?,
        treasury: f.address("treasury")?,
        fee_bps: f.u32("fee_bps")?,
        grace_secs: f.u64("grace_secs")?,
        min_duration_secs: f.u64("min_duration_secs")?,
        max_duration_secs: f.u64("max_duration_secs")?,
        paused: f.bool("paused")?,
    })
}

#[async_trait]
impl ChainReader for SorobanChain {
    async fn get_bounty(&self, id: u64) -> Result<ChainBounty, ChainError> {
        parse_bounty(&self.read("get_bounty", vec![scval::u64v(id)]).await?)
    }

    async fn get_config(&self) -> Result<ContractConfig, ChainError> {
        parse_config(&self.read("get_config", vec![]).await?)
    }

    async fn latest_ledger(&self) -> Result<u32, ChainError> {
        let w: LatestLedgerWire = self.rpc.call("getLatestLedger", json!({})).await?;
        Ok(w.sequence)
    }
}

fn page(w: EventsWire, fallback_cursor: &str) -> EventsPage {
    let events = w
        .events
        .into_iter()
        .map(|e| RawEvent {
            id: e.id,
            ledger: e.ledger,
            tx_hash: e.tx_hash,
            ledger_closed_at: e.ledger_closed_at,
            topics_b64: e.topic,
            value_b64: e.value,
            in_successful_contract_call: e.in_successful_contract_call,
        })
        .collect();
    EventsPage { events, cursor: w.cursor.unwrap_or_else(|| fallback_cursor.to_string()), latest_ledger: w.latest_ledger }
}

#[async_trait]
impl EventSource for SorobanChain {
    async fn events_from_ledger(&self, start_ledger: u32, limit: u32) -> Result<EventsPage, ChainError> {
        let params = json!({
            "startLedger": start_ledger,
            "filters": [{ "type": "contract", "contractIds": [self.contract_id()] }],
            "pagination": { "limit": limit },
        });
        let w: EventsWire = self.rpc.call("getEvents", params).await?;
        Ok(page(w, ""))
    }

    async fn events_from_cursor(&self, cursor: &str, limit: u32) -> Result<EventsPage, ChainError> {
        let params = json!({
            "filters": [{ "type": "contract", "contractIds": [self.contract_id()] }],
            "pagination": { "cursor": cursor, "limit": limit },
        });
        let w: EventsWire = self.rpc.call("getEvents", params).await?;
        Ok(page(w, cursor))
    }
}

#[async_trait]
impl TxBuilder for SorobanChain {
    async fn build_fund(&self, p: &FundParams) -> Result<String, ChainError> {
        let args = vec![
            scval::address(&p.maintainer)?,
            scval::u64v(p.repo_id),
            scval::u64v(p.issue),
            scval::address(&p.token)?,
            scval::i128v(p.amount),
            scval::u64v(p.deadline),
            scval::boolv(p.relayer_allowed),
        ];
        self.prepare(&p.maintainer, "fund_bounty", args).await
    }

    async fn build_release(&self, maintainer: &str, bounty_id: u64, winner: &str) -> Result<String, ChainError> {
        let args = vec![scval::address(maintainer)?, scval::u64v(bounty_id), scval::address(winner)?];
        self.prepare(maintainer, "release", args).await
    }

    async fn build_refund(&self, maintainer: &str, bounty_id: u64) -> Result<String, ChainError> {
        self.prepare(maintainer, "refund", vec![scval::address(maintainer)?, scval::u64v(bounty_id)]).await
    }

    async fn build_open_dispute(&self, arbiter: &str, bounty_id: u64) -> Result<String, ChainError> {
        self.prepare(arbiter, "open_dispute", vec![scval::address(arbiter)?, scval::u64v(bounty_id)]).await
    }

    async fn build_resolve(&self, arbiter: &str, bounty_id: u64, winner: Option<&str>, winner_bps: u32) -> Result<String, ChainError> {
        let args = vec![scval::address(arbiter)?, scval::u64v(bounty_id), scval::opt_address(winner)?, scval::u32v(winner_bps)];
        self.prepare(arbiter, "resolve", args).await
    }

    async fn submit_signed(&self, xdr: &str, expected_source: Option<&str>) -> Result<String, ChainError> {
        let inspected = tx::inspect_submission(xdr, self.network_id, &self.contract, expected_source)?;
        self.send(xdr).await?;
        Ok(inspected.hash)
    }
}

/// Latest ledger sequence reached by a page (cursor-based), used by the indexer to track progress on quiet contracts.
pub fn page_ledger(p: &EventsPage) -> u32 {
    cursor_ledger(&p.cursor)
}
