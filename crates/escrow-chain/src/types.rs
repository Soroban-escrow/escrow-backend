use escrow_core::BountyStatus;
use serde::Serialize;

/// The contract's `Bounty`, as read from the chain. Money decisions use this, never the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBounty {
    pub id: u64,
    pub maintainer: String,
    pub repo_id: u64,
    pub issue: u64,
    pub token: String,
    pub amount: i128,
    pub fee_bps: u32,
    pub deadline: u64,
    pub relayer_allowed: bool,
    pub status: BountyStatus,
    pub winner: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractConfig {
    pub admin: String,
    pub relayer: String,
    pub arbiter: String,
    pub treasury: String,
    pub fee_bps: u32,
    pub grace_secs: u64,
    pub min_duration_secs: u64,
    pub max_duration_secs: u64,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct FundParams {
    pub maintainer: String,
    pub repo_id: u64,
    pub issue: u64,
    pub token: String,
    pub amount: i128,
    pub deadline: u64,
    pub relayer_allowed: bool,
}

/// One page of contract events plus where to resume.
#[derive(Debug, Clone)]
pub struct EventsPage {
    pub events: Vec<crate::events::RawEvent>,
    /// opaque resume token; its high 32 bits are the ledger it reached
    pub cursor: String,
    pub latest_ledger: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome {
    Success,
    Failed,
    /// Never seen on the network within the wait window (it may still land, or it may have expired).
    NotFound,
}
