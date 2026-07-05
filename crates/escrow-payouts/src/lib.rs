#![forbid(unsafe_code)]
//! Payouts: from "a linked PR was merged" to "the contributor was paid on chain".
//!
//! The rules this crate exists to keep:
//!
//! * **The chain decides.** Every money decision re-reads the bounty from the chain; the database is
//!   only bookkeeping. A payout whose release already landed is recognised and confirmed, never repeated.
//! * **One payout per bounty.** The idempotency key `release:<bounty_id>` is unique: the first merge wins.
//! * **Hash before wait.** The submitted transaction hash is stored *before* anyone waits on it, so a
//!   crash can never lead to a second submission.
//! * **No AI in here.** This crate does not depend on the AI reviewer (a test enforces it): a review
//!   can never move money.

pub mod loop_;
pub mod payout;
pub mod ports;

pub use loop_::{RetryTable, backoff, relayer_pass, run_relayer};
pub use payout::{Advance, MAX_ATTEMPTS, MergeOutcome, PayoutDeps, RelayResult, advance, on_merge, on_wallet_linked, relay_release};
pub use ports::{Notifier, PayoutError};
