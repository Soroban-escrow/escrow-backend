#![forbid(unsafe_code)]
//! The application layer: everything that turns chain events and GitHub webhooks into state and action.
//!
//! * [`indexer`] folds contract events into the database (the only writer of bounty status).
//! * [`dispatch`], [`commands`], [`pulls`] handle verified GitHub webhooks.
//! * [`sync`] keeps the bounty card, labels and "Paid" comments on GitHub in step with the chain.
//! * [`ops`] is health, alerting and reminders.
//! * [`worker`] runs it all: a Postgres-backed job queue plus the long-running loops.
//!
//! This crate does not depend on the AI reviewer. Reviews are requested and run through the
//! [`ports::ReviewScheduler`] / [`ports::ReviewRunner`] seams, which only the binary wires up.

pub mod commands;
pub mod dispatch;
pub mod error;
pub mod indexer;
pub mod jobs;
pub mod ops;
pub mod ports;
pub mod pulls;
pub mod sync;
pub mod worker;

pub use error::EngineError;
