#![forbid(unsafe_code)]
//! Pure domain logic for SkillEscrow: no network, no database, no clock reads.
//!
//! Everything here is deterministic and unit-tested, so the money rules (fees, splits, amounts),
//! the parsers that read untrusted GitHub text, and the security primitives (webhook signatures,
//! CSRF tokens, OAuth state, wallet-link challenges) can be reviewed without any infrastructure.

pub mod amounts;
pub mod card;
pub mod command;
pub mod config;
pub mod contract_errors;
pub mod criteria;
pub mod fees;
pub mod oauth;
pub mod refs;
pub mod security;
pub mod status;

pub use amounts::{AmountError, TOKEN_DECIMALS, from_base_units, to_base_units};
pub use status::{BountyStatus, DisputeStatus, PayoutStatus};

/// Sent by every outbound HTTP client, so a provider can tell which release made a request.
pub const USER_AGENT: &str = concat!("skillescrow-backend/", env!("CARGO_PKG_VERSION"));

/// Dispute response window SLA in hours. Promised to disputants, used for overdue flagging and alerts.
/// The alert fires at 36h (before the 48h SLA) so operators have warning before the deadline.
pub const DISPUTE_RESPONSE_HOURS: i64 = 48;
/// Hours before the SLA when the overdue alert/flag fires, giving operators advance warning.
pub const DISPUTE_ALERT_HOURS: i64 = 36;
