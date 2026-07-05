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

impl Relayer {
    pub fn new(chain: SorobanChain, secret: &str) -> Result<Self, ChainError> {
        Ok(Self { chain, signer: Signer::from_secret(secret)? })
    }
}

#[async_trait]
impl ReleaseSigner for Relayer {
    fn address(&self) -> String {
        self.signer.address()
    }

    async fn submit_release(&self, bounty_id: u64, winner: &str) -> Result<String, ChainError> {
        let me = self.signer.address();
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
        // a contract rejection (wrong status, relayer not allowed, ...) surfaces here, before anything is signed
        let sim = self.chain.simulate(&t).await?;
        let (env, hash) = tx::sign(tx::assemble(t, &sim)?, self.chain.network_id(), &self.signer)?;
        self.chain.send(&tx::to_b64(&env)?).await?;
        Ok(hex::encode(hash))
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
