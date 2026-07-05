use async_trait::async_trait;
use escrow_chain::{ChainBounty, ChainError, ChainReader, ContractConfig, EventSource, EventsPage, FundParams, RawEvent, TxBuilder};
use escrow_core::BountyStatus;
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
pub struct FakeChain {
    pub bounties: Mutex<HashMap<u64, ChainBounty>>,
    pub ledger: Mutex<u32>,
    pub reads: Mutex<u32>,
}

impl FakeChain {
    pub fn put(&self, id: u64, maintainer: &str, status: BountyStatus, relayer_allowed: bool, winner: Option<&str>) {
        self.bounties.lock().unwrap().insert(
            id,
            ChainBounty {
                id,
                maintainer: maintainer.into(),
                repo_id: 10,
                issue: 3,
                token: "CTOKEN".into(),
                amount: 50_000_000,
                fee_bps: 300,
                deadline: 0,
                relayer_allowed,
                status,
                winner: winner.map(Into::into),
                created_at: 0,
            },
        );
    }

    pub fn set_status(&self, id: u64, status: BountyStatus) {
        if let Some(b) = self.bounties.lock().unwrap().get_mut(&id) {
            b.status = status;
        }
    }
}

#[async_trait]
impl ChainReader for FakeChain {
    async fn get_bounty(&self, id: u64) -> Result<ChainBounty, ChainError> {
        *self.reads.lock().unwrap() += 1;
        self.bounties.lock().unwrap().get(&id).cloned().ok_or_else(|| ChainError::Simulation("HostError: Error(Contract, #10)".into()))
    }
    async fn get_config(&self) -> Result<ContractConfig, ChainError> {
        Ok(ContractConfig {
            admin: "GADMIN".into(),
            relayer: "GRELAYER".into(),
            arbiter: "GARBITER".into(),
            treasury: "GTREASURY".into(),
            fee_bps: 300,
            grace_secs: 259_200,
            min_duration_secs: 86_400,
            max_duration_secs: 7_776_000,
            paused: false,
        })
    }
    async fn latest_ledger(&self) -> Result<u32, ChainError> {
        Ok(*self.ledger.lock().unwrap())
    }
}

/// Events served page by page. `from_ledger` / `from_cursor` calls are recorded.
#[derive(Default)]
pub struct FakeEvents {
    pub pages: Mutex<Vec<Result<EventsPage, ChainError>>>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeEvents {
    pub fn push_page(&self, events: Vec<RawEvent>, cursor: &str, latest_ledger: u32) {
        self.pages.lock().unwrap().push(Ok(EventsPage { events, cursor: cursor.into(), latest_ledger }));
    }
    pub fn push_error(&self, e: ChainError) {
        self.pages.lock().unwrap().push(Err(e));
    }
    fn next(&self, what: String) -> Result<EventsPage, ChainError> {
        self.calls.lock().unwrap().push(what);
        let mut p = self.pages.lock().unwrap();
        if p.is_empty() {
            return Err(ChainError::Invalid("FakeEvents: no more pages scripted".into()));
        }
        p.remove(0)
    }
}

#[async_trait]
impl EventSource for FakeEvents {
    async fn events_from_ledger(&self, start_ledger: u32, _limit: u32) -> Result<EventsPage, ChainError> {
        self.next(format!("ledger:{start_ledger}"))
    }
    async fn events_from_cursor(&self, cursor: &str, _limit: u32) -> Result<EventsPage, ChainError> {
        self.next(format!("cursor:{cursor}"))
    }
}

/// Records what the API asked it to build; returns recognisable fake XDR.
#[derive(Default)]
pub struct FakeTxBuilder {
    pub built: Mutex<Vec<String>>,
    pub submitted: Mutex<Vec<(String, Option<String>)>>,
    pub fail_with: Mutex<Option<ChainError>>,
}

impl FakeTxBuilder {
    fn fail(&self) -> Result<(), ChainError> {
        self.fail_with.lock().unwrap().take().map_or(Ok(()), Err)
    }
    fn record(&self, s: String) -> Result<String, ChainError> {
        self.fail()?;
        self.built.lock().unwrap().push(s.clone());
        Ok(format!("XDR[{s}]"))
    }
}

#[async_trait]
impl TxBuilder for FakeTxBuilder {
    async fn build_fund(&self, p: &FundParams) -> Result<String, ChainError> {
        self.record(format!("fund:{}:{}:{}:{}:{}:{}", p.maintainer, p.repo_id, p.issue, p.token, p.amount, p.relayer_allowed))
    }
    async fn build_release(&self, maintainer: &str, bounty_id: u64, winner: &str) -> Result<String, ChainError> {
        self.record(format!("release:{maintainer}:{bounty_id}:{winner}"))
    }
    async fn build_refund(&self, maintainer: &str, bounty_id: u64) -> Result<String, ChainError> {
        self.record(format!("refund:{maintainer}:{bounty_id}"))
    }
    async fn build_open_dispute(&self, arbiter: &str, bounty_id: u64) -> Result<String, ChainError> {
        self.record(format!("open_dispute:{arbiter}:{bounty_id}"))
    }
    async fn build_resolve(&self, arbiter: &str, bounty_id: u64, winner: Option<&str>, winner_bps: u32) -> Result<String, ChainError> {
        self.record(format!("resolve:{arbiter}:{bounty_id}:{}:{winner_bps}", winner.unwrap_or("-")))
    }
    async fn submit_signed(&self, xdr: &str, expected_source: Option<&str>) -> Result<String, ChainError> {
        self.fail()?;
        self.submitted.lock().unwrap().push((xdr.into(), expected_source.map(Into::into)));
        Ok("f".repeat(64))
    }
}
