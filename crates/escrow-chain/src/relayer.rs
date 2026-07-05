//! The relayer: signs and submits `release(relayer, bounty_id, winner)` for bounties whose
//! maintainer opted in. It is the only key the server holds, and the contract limits what it can do:
//! it can only release an Open bounty with `relayer_allowed`, and never to the maintainer.
//!
//! On mainnet this must be replaced by a KMS-backed [`ReleaseSigner`]; the config refuses a raw
//! secret there.

use crate::{
    client::{RELAYER_TX_VALID_SECS, SorobanChain},
    error::ChainError,
    keys::Signer,
    ports::ReleaseSigner,
    scval, tx,
    types::TxOutcome,
};
use async_trait::async_trait;
use std::time::Duration;

pub struct Relayer {
    chain: SorobanChain,
    signer: Signer,
}

/// A signed `release` transaction, ready to send. Nothing has been sent yet.
#[derive(Debug, Clone)]
pub struct PreparedRelease {
    pub envelope_b64: String,
    pub hash: [u8; 32],
}

impl Relayer {
    pub fn new(chain: SorobanChain, secret: &str) -> Result<Self, ChainError> {
        Ok(Self { chain, signer: Signer::from_secret(secret)? })
    }

    /// Builds, simulates (so a contract rejection surfaces before anything is signed), assembles and signs
    /// the release. Sends nothing. If the contract's state has been archived it is restored first (once).
    pub async fn prepare_release(&self, bounty_id: u64, winner: &str) -> Result<PreparedRelease, ChainError> {
        let me = self.signer.address();
        for attempt in 0..2 {
            let (seq, _) = self.chain.account(&me).await?;
            let args = vec![scval::address(&me)?, scval::u64v(bounty_id), scval::address(winner)?];
            let t = tx::invoke_tx(
                self.signer.public_bytes(),
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
            let sim = wire.into_ok()?;
            let (env, hash) = tx::sign(tx::assemble(t, &sim)?, self.chain.network_id(), &self.signer)?;
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
        match self.chain.wait_for_tx(&hex::encode(hash), Duration::from_secs(60)).await? {
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
        let prepared = self.prepare_release(bounty_id, winner).await?;
        self.chain.send(&prepared.envelope_b64).await?;
        Ok(hex::encode(prepared.hash))
    }

    async fn wait_for_tx(&self, hash: &str) -> Result<TxOutcome, ChainError> {
        self.chain.wait_for_tx(hash, Duration::from_secs(60)).await
    }

    async fn balance_xlm(&self) -> Result<f64, ChainError> {
        let (_, stroops) = self.chain.account(&self.signer.address()).await?;
        Ok(stroops as f64 / 10_000_000.0)
    }
}

/// Exposed for diagnostics (`escrowd doctor`): the relayer's public address from its secret.
pub fn address_of_secret(secret: &str) -> Result<String, ChainError> {
    Ok(Signer::from_secret(secret)?.address())
}
