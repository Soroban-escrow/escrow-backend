#![forbid(unsafe_code)]
//! In-memory fakes shared by the test suites of several crates. Dev-only (`publish = false`).
//! They implement the same ports production code depends on, so tests exercise the real logic with no network.

pub mod chain;
pub mod github;
pub mod seed;

pub use chain::{FakeChain, FakeEvents, FakeTxBuilder};
pub use github::FakeGithub;
