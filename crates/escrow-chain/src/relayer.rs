//! The relayer: signs and submits `release(relayer, bounty_id, winner)` for bounties whose
//! maintainer opted in. It is the only key the server holds, and the contract limits what it can do:
//! it can only release an Open bounty with `relayer_allowed`, and never to the maintainer.
//!
//! Without channel accounts the relayer is also the transaction source (one release at a time). With channel
//! accounts, each release is sourced from an idle channel and the relayer signs only its own authorisation
//! (see [`crate::channels`]).
//!
//! On mainnet this must be replaced by a KMS-backed [`ReleaseSigner`]; the config refuses a raw
//! secret there.

use crate::{
    channels::{self, ChannelPool},
    client::{RELAYER_TX_VALID_SECS, SorobanChain},
    error::ChainError,
    keys::Signer,
    ports::ReleaseSigner,
    scval, tx,
    types::TxOutcome,
};
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};

pub struct Relayer {
    chain: SorobanChain,
    signer: Signer,
    channels: Option<Arc<ChannelPool>>,
    /// #113: how long `wait_for_tx` polls for a submitted transaction to resolve, and the
    /// window a restore waits before giving up. Defaults to 60 s; tune via
    /// `RELAYER_CONFIRM_WAIT_SECS` (bounded to the submit validity window).
    confirm_wait: Duration,
}

/// Same 60 s wait the relayer used before #113 became configurable. Exposed so adjacent
/// call sites (tests, admin tools, future signers) can match the production default
/// without re-declaring the constant.
pub const DEFAULT_RELAYER_CONFIRM_WAIT: Duration = Duration::from_secs(60);

/// A signed `release` transaction, ready to send. Nothing has been sent yet.
#[derive(Debug, Clone)]
pub struct PreparedRelease {
    pub envelope_b64: String,
    pub hash: [u8; 32],
}

impl Relayer {
    pub fn new(chain: SorobanChain, secret: &str) -> Result<Self, ChainError> {
        Ok(Self { chain, signer: Signer::from_secret(secret)?, channels: None, confirm_wait: DEFAULT_RELAYER_CONFIRM_WAIT })
    }

    /// Adds channel accounts (`RELAYER_CHANNEL_SECRETS`). An empty list keeps the relayer as its own source.
    pub fn with_channel_secrets(mut self, secrets: &[String]) -> Result<Self, ChainError> {
        if !secrets.is_empty() {
            self.channels = Some(Arc::new(ChannelPool::new(channels::signers_from_secrets(secrets)?)?));
        }
        Ok(self)
    }

    /// #113: overrides the confirmation wait. Returns `self` so this composes with
    /// `with_channel_secrets` in the builder chain the runtime uses.
    pub fn with_confirm_wait(mut self, wait: Duration) -> Self {
        self.confirm_wait = wait;
        self
    }

    /// Builds, simulates (so a contract rejection surfaces before anything is signed), assembles and signs
    /// the release from the relayer's own account. Sends nothing. If the contract's state has been archived it is
    /// restored first (once).
    pub async fn prepare_release(&self, bounty_id: u64, winner: &str) -> Result<PreparedRelease, ChainError> {
        self.prepare_from(&self.signer, bounty_id, winner).await
    }

    /// Like [`Self::prepare_release`], but the transaction is sourced from `source`. When `source` is a channel,
    /// the relayer's authorisation entries are signed here, so the relayer still approves the call itself.
    async fn prepare_from(&self, source: &Signer, bounty_id: u64, winner: &str) -> Result<PreparedRelease, ChainError> {
        let me = self.signer.address();
        let via_channel = source.public_bytes() != self.signer.public_bytes();
        for attempt in 0..2 {
            let (seq, _) = self.chain.account(&source.address()).await?;
            let args = vec![scval::address(&me)?, scval::u64v(bounty_id), scval::address(winner)?];
            let t = tx::invoke_tx(
                source.public_bytes(),
                seq + 1,
                self.chain.contract,
                "release",
                args,
                tx::now_secs() + RELAYER_TX_VALID_SECS,
            )?;
            let wire = self.chain.simulate_wire(&t).await?;
            if let Some(pre) = wire.restore_preamble()? {
                if attempt == 1 {
                    return Err(ChainError::Archived);
                }
                self.restore(pre).await?;
                continue;
            }
            let mut sim = wire.into_ok()?;
            if via_channel {
                for entry in sim.auth.iter_mut() {
                    channels::authorize_for_relayer(entry, &self.signer, self.chain.network_id())?;
                }
            }
            let (env, hash) = tx::sign(tx::assemble(t, &sim)?, self.chain.network_id(), source)?;
            return Ok(PreparedRelease { envelope_b64: tx::to_b64(&env)?, hash });
        }
        Err(ChainError::Archived)
    }

    /// Restores archived state and waits for it to land. Anyone may pay to restore; the relayer does.
    async fn restore(&self, pre: crate::rpc::RestorePreamble) -> Result<(), ChainError> {
        let (seq, _) = self.chain.account(&self.signer.address()).await?;
        let t = tx::restore_tx(
            self.signer.public_bytes(),
            seq + 1,
            pre.transaction_data,
            pre.min_resource_fee,
            tx::now_secs() + RELAYER_TX_VALID_SECS,
        )?;
        let (env, hash) = tx::sign(t, self.chain.network_id(), &self.signer)?;
        self.chain.send(&tx::to_b64(&env)?).await?;
        tracing::info!(tx = %hex::encode(hash), "restoring archived contract state");
        match self.chain.wait_for_tx(&hex::encode(hash), self.confirm_wait).await? {
            TxOutcome::Success => Ok(()),
            _ => Err(ChainError::Rejected("restoring archived state did not succeed".into())),
        }
    }
}

#[async_trait]
impl ReleaseSigner for Relayer {
    fn address(&self) -> String {
        self.signer.address()
    }

    async fn submit_release(&self, bounty_id: u64, winner: &str) -> Result<String, ChainError> {
        let Some(pool) = &self.channels else {
            let prepared = self.prepare_release(bounty_id, winner).await?;
            self.chain.send(&prepared.envelope_b64).await?;
            return Ok(hex::encode(prepared.hash));
        };
        // every channel busy: a transient condition, the payout is retried shortly
        let lease = pool.lease().ok_or_else(|| ChainError::Http("every relayer channel is busy".into()))?;
        let prepared = self.prepare_from(lease.signer(), bounty_id, winner).await?;
        match self.chain.send(&prepared.envelope_b64).await {
            Ok(_) => Ok(hex::encode(prepared.hash)),
            Err(e) => {
                // we cannot tell whether the network took the transaction, so its channel's sequence is suspect
                tracing::error!(channel = %lease.signer().address(), error = %e, "channel quarantined after a failed submission");
                lease.quarantine();
                Err(e)
            }
        }
    }

    async fn wait_for_tx(&self, hash: &str) -> Result<TxOutcome, ChainError> {
        self.chain.wait_for_tx(hash, self.confirm_wait).await
    }

    async fn balance_xlm(&self) -> Result<f64, ChainError> {
        let (_, stroops) = self.chain.account(&self.signer.address()).await?;
        Ok(stroops as f64 / 10_000_000.0)
    }

    fn max_parallel(&self) -> usize {
        self.channels.as_ref().map_or(1, |p| p.len())
    }
}

/// Exposed for diagnostics (`escrowd doctor`): the relayer's public address from its secret.
pub fn address_of_secret(secret: &str) -> Result<String, ChainError> {
    Ok(Signer::from_secret(secret)?.address())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChainSettings;

    // StrKey ed25519 seed from a 32-byte repeated value - same pattern as the rpc_fake tests
    // use, which keeps unit tests free of any real key material.
    fn test_secret(seed: u8) -> String {
        stellar_strkey::Unredacted(stellar_strkey::ed25519::PrivateKey([seed; 32])).to_string()
    }

    fn chain() -> SorobanChain {
        SorobanChain::new(&ChainSettings::new(
            "http://127.0.0.1:1/unused",
            "Test SDF Network ; September 2015",
            "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V",
        ))
        .expect("ChainSettings constructs")
    }

    #[test]
    fn confirm_wait_defaults_to_sixty_seconds_and_is_overridable() {
        let secret = test_secret(1);
        let r = Relayer::new(chain(), &secret).expect("relayer builds");
        assert_eq!(r.confirm_wait, DEFAULT_RELAYER_CONFIRM_WAIT);
        assert_eq!(r.confirm_wait, Duration::from_secs(60));

        // #113: tests prefer a short window so a failing network does not pin the thread for a minute.
        let short = Relayer::new(chain(), &secret).unwrap().with_confirm_wait(Duration::from_millis(50));
        assert_eq!(short.confirm_wait, Duration::from_millis(50));
    }

    #[test]
    fn with_confirm_wait_composes_with_channel_secrets() {
        // Builder order stays flexible: setting the wait does not clobber channels, and
        // vice versa. The runtime relies on this to apply both from Config in one chain.
        let secret = test_secret(2);
        let r = Relayer::new(chain(), &secret)
            .unwrap()
            .with_channel_secrets(&[])
            .unwrap()
            .with_confirm_wait(Duration::from_millis(10));
        assert_eq!(r.confirm_wait, Duration::from_millis(10));
        assert!(r.channels.is_none());
    }
}
