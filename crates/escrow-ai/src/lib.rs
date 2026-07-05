#![forbid(unsafe_code)]
//! The AI pull-request reviewer. **Advisory only.**
//!
//! This crate reads PRs and writes one comment per PR. It has no way to move money, and nothing in the
//! payout path depends on it: `escrow-payouts`, `escrow-chain` and `escrow-engine` do not link this
//! crate (a workspace test enforces that). A review that is wrong, missing, or manipulated by hostile
//! diff text can change what a maintainer reads, never what gets paid.
//!
//! Defences, in layers: untrusted text is fenced and defanged before it reaches the model; the model
//! is told to treat it as data and to flag injection attempts; its output must match a strict schema
//! or the review becomes "needs a human"; and code (not the model) downgrades any review that reports
//! an injection attempt.

pub mod anthropic;
pub mod eval;
pub mod input;
pub mod pipeline;
pub mod review;
pub mod schema;

pub use anthropic::AnthropicModel;
pub use pipeline::{ReviewDeps, ReviewOutcome, run_pr_review};
pub use review::{ModelError, ModelOutput, ModelPort};
