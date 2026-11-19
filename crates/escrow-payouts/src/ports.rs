use async_trait::async_trait;
use escrow_chain::ChainError;
use thiserror::Error;

/// Posts a comment about a payout (on the PR when there is one, otherwise on the issue).
/// Best effort: a failed comment must never block or fail a payout.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, bounty_id: i64, pr_number: i32, message: &str) -> Result<(), String>;
}

#[derive(Debug, Error)]
pub enum PayoutError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// A retryable relayer failure: the caller backs off and tries again.
    #[error("transient: {0}")]
    Transient(String),
    #[error("{0}")]
    Other(String),
}
