//! The seams the rest of the backend depends on. Production wires in [`crate::SorobanChain`] and
//! [`crate::Relayer`]; tests wire in in-memory fakes, so no test needs a network.

use crate::{
    error::ChainError,
    types::{ChainBounty, ContractConfig, EventsPage, FundParams, TxOutcome},
};
use async_trait::async_trait;

/// Reads from the chain. Payout code uses this and never the database to decide about money.
#[async_trait]
pub trait ChainReader: Send + Sync {
    async fn get_bounty(&self, id: u64) -> Result<ChainBounty, ChainError>;
    async fn get_config(&self) -> Result<ContractConfig, ChainError>;
    async fn latest_ledger(&self) -> Result<u32, ChainError>;
}

#[async_trait]
pub trait EventSource: Send + Sync {
    async fn events_from_ledger(&self, start_ledger: u32, limit: u32) -> Result<EventsPage, ChainError>;
    async fn events_from_cursor(&self, cursor: &str, limit: u32) -> Result<EventsPage, ChainError>;
}

/// Prepares unsigned transactions for a user's wallet to sign, and relays what they signed.
/// Keys other than the relayer's never touch the server.
#[async_trait]
pub trait TxBuilder: Send + Sync {
    async fn build_fund(&self, p: &FundParams) -> Result<String, ChainError>;
    async fn build_release(&self, maintainer: &str, bounty_id: u64, winner: &str) -> Result<String, ChainError>;
    async fn build_refund(&self, maintainer: &str, bounty_id: u64) -> Result<String, ChainError>;
    async fn build_open_dispute(&self, arbiter: &str, bounty_id: u64) -> Result<String, ChainError>;
    async fn build_resolve(&self, arbiter: &str, bounty_id: u64, winner: Option<&str>, winner_bps: u32) -> Result<String, ChainError>;
    /// Accepts only a signed, single-operation call to *our* contract. Returns the transaction hash.
    async fn submit_signed(&self, xdr: &str, expected_source: Option<&str>) -> Result<String, ChainError>;
}

/// The relayer: the only key the server holds. It can call `release` and nothing else.
#[async_trait]
pub trait ReleaseSigner: Send + Sync {
    fn address(&self) -> String;
    /// Signs and submits `release(relayer, bounty_id, winner)`; returns the hash (stored BEFORE waiting).
    async fn submit_release(&self, bounty_id: u64, winner: &str) -> Result<String, ChainError>;
    async fn wait_for_tx(&self, hash: &str) -> Result<TxOutcome, ChainError>;
    async fn balance_xlm(&self) -> Result<f64, ChainError>;
}
