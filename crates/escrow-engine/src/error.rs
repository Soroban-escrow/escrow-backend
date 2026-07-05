use escrow_chain::ChainError;
use escrow_github::GithubError;
use escrow_payouts::PayoutError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("github: {0}")]
    Github(#[from] GithubError),
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    #[error("payout: {0}")]
    Payout(#[from] PayoutError),
    #[error("bad webhook payload: {0}")]
    Payload(String),
    #[error("{0}")]
    Other(String),
}

impl EngineError {
    /// Worth retrying later. A bad payload or a permission failure will fail the same way next time.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Db(_) => true,
            Self::Github(e) => e.is_transient(),
            Self::Chain(e) => e.is_transient(),
            Self::Payout(PayoutError::Db(_) | PayoutError::Transient(_)) => true,
            Self::Payout(PayoutError::Chain(e)) => e.is_transient(),
            Self::Payout(PayoutError::Other(_)) | Self::Payload(_) | Self::Other(_) => false,
        }
    }
}
