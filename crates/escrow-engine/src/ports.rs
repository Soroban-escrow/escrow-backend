//! Seams for the AI reviewer. The engine never links it; the binary provides the implementations.

use async_trait::async_trait;
use std::time::Duration;

/// Asks for a PR review to happen later (debounced) or now (forced).
#[async_trait]
pub trait ReviewScheduler: Send + Sync {
    async fn schedule(&self, pr_id: i64, delay: Duration, force: bool) -> Result<(), String>;
}

#[derive(Debug)]
pub struct ReviewFailure {
    pub message: String,
    pub transient: bool,
}

/// Performs one review. Returns a short outcome label for logs.
#[async_trait]
pub trait ReviewRunner: Send + Sync {
    async fn run(&self, pr_id: i64, force: bool) -> Result<String, ReviewFailure>;
}
