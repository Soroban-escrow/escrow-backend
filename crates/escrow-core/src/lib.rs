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
