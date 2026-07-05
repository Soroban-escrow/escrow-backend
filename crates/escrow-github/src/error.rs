use thiserror::Error;

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("GitHub returned {status} for {what}")]
    Status { status: u16, what: String },
    #[error("network error talking to GitHub: {0}")]
    Network(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("could not read GitHub's response: {0}")]
    Decode(String),
    #[error("GitHub App is not configured (see docs/GITHUB_APP.md)")]
    NotConfigured,
}

impl GithubError {
    /// Worth retrying later: server trouble or rate limiting, not a permission or validation failure.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => *status >= 500 || *status == 429,
            Self::Network(_) => true,
            // configuration can be fixed while a job is still retrying
            Self::NotConfigured => true,
            _ => false,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for GithubError {
    fn from(e: reqwest::Error) -> Self {
        // keep only the kind: the URL may contain an installation id or a token-bearing query
        Self::Network(
            if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connection failed"
            } else {
                "request failed"
            }
            .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_means_server_side_or_rate_limit() {
        for s in [500, 502, 503, 429] {
            assert!(GithubError::Status { status: s, what: "x".into() }.is_transient(), "{s}");
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(!GithubError::Status { status: s, what: "x".into() }.is_transient(), "{s}");
        }
        assert!(GithubError::Network("timeout".into()).is_transient());
        assert!(GithubError::NotConfigured.is_transient(), "jobs wait for the App to be configured instead of dying");
        assert!(!GithubError::Invalid("x".into()).is_transient());
    }
}
