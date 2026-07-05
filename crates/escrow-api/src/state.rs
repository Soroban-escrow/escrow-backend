use crate::{limiter::RateLimiter, login::GithubLogin, metrics::Metrics};
use chrono::{DateTime, Utc};
use escrow_chain::{ChainReader, TxBuilder};
use escrow_db::Db;
use escrow_engine::sync::Tokens;
use escrow_payouts::PayoutDeps;
use std::sync::Arc;

/// What the HTTP layer needs from configuration.
#[derive(Clone)]
pub struct ApiConfig {
    pub csrf_secret: String,
    pub session_secret: String,
    pub webhook_secret: String,
    pub api_public_url: String,
    pub web_public_url: String,
    pub secure_cookies: bool,
    pub admin_github_ids: Vec<i64>,
    pub trust_proxy: bool,
    pub metrics_token: Option<String>,
    pub explorer_tx_template: String,
    pub tokens: Tokens,
    pub network: String,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub cfg: Arc<ApiConfig>,
    pub chain: Arc<dyn ChainReader>,
    pub txb: Arc<dyn TxBuilder>,
    /// absent until the GitHub App's OAuth credentials are configured
    pub login: Option<Arc<dyn GithubLogin>>,
    pub payouts: PayoutDeps,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
    pub now: fn() -> DateTime<Utc>,
}
