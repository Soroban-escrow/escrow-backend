//! Wires configuration into running parts. This is the only place that knows about every crate, and the
//! only place the AI reviewer is connected to anything.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration as Chrono, Utc};
use escrow_ai::{AnthropicModel, ReviewDeps, run_pr_review};
use escrow_api::{ApiConfig, AppState, limiter::RateLimiter, login::RealLogin, metrics::Metrics};
use escrow_chain::{ChainReader, ChainSettings, EventSource, Relayer, ReleaseSigner, SorobanChain, TxBuilder};
use escrow_core::config::Config;
use escrow_db::Db;
use escrow_engine::{
    commands::{CommandDeps, TokenTable},
    dispatch::DispatchDeps,
    indexer::IndexerDeps,
    jobs::JobReviewScheduler,
    ops::AlertSender,
    ports::{ReviewFailure, ReviewRunner, ReviewScheduler},
    pulls::PullDeps,
    sync::{GithubNotifier, SyncDeps, Tokens},
    worker::WorkerDeps,
};
use escrow_github::{
    GithubApi, GithubClient, GithubClientConfig, GithubError, InstallationInfo, Issue, OAuthClient, Pull, PullFile, RepoInfo,
};
use escrow_payouts::PayoutDeps;
use std::{sync::Arc, time::Duration};

/// Stands in for GitHub when the App is not configured: every call fails with a retryable error that
/// says so, so jobs wait for the configuration instead of silently doing nothing.
struct NoGithub;

#[async_trait]
impl GithubApi for NoGithub {
    async fn get_permission(&self, _i: i64, _r: &str, _l: &str) -> Result<String, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn post_comment(&self, _i: i64, _r: &str, _n: i32, _b: &str) -> Result<i64, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn update_comment(&self, _i: i64, _r: &str, _c: i64, _b: &str) -> Result<(), GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn get_issue(&self, _i: i64, _r: &str, _n: i32) -> Result<Issue, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn set_bounty_label(&self, _i: i64, _r: &str, _n: i32, _l: &str) -> Result<(), GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn get_pull(&self, _i: i64, _r: &str, _n: i32) -> Result<Pull, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn get_pull_files(&self, _i: i64, _r: &str, _n: i32) -> Result<Vec<PullFile>, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn get_ci_conclusion(&self, _i: i64, _r: &str, _s: &str) -> Result<escrow_github::CiConclusion, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn get_readme_excerpt(&self, _i: i64, _r: &str, _m: usize) -> Result<String, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn list_installations(&self) -> Result<Vec<InstallationInfo>, GithubError> {
        Err(GithubError::NotConfigured)
    }
    async fn list_repositories(&self, _i: i64) -> Result<Vec<RepoInfo>, GithubError> {
        Err(GithubError::NotConfigured)
    }
}

struct AiRunner(ReviewDeps);

#[async_trait]
impl ReviewRunner for AiRunner {
    async fn run(&self, pr_id: i64, force: bool) -> Result<String, ReviewFailure> {
        run_pr_review(&self.0, Utc::now(), pr_id, force)
            .await
            .map(|o| format!("{o:?}"))
            .map_err(|e| ReviewFailure { transient: e.is_transient(), message: e.to_string() })
    }
}

pub struct Runtime {
    pub cfg: Config,
    pub db: Db,
    pub chain: SorobanChain,
    pub gh: Arc<dyn GithubApi>,
    pub github_configured: bool,
    pub relayer: Option<Arc<Relayer>>,
    pub ai_enabled: bool,
}

pub async fn connect(cfg: Config, migrate: bool) -> Result<Runtime> {
    let db = Db::connect(&cfg.database_url, cfg.db_max_connections).await.context("could not connect to the database")?;
    if migrate {
        db.migrate().await.context("could not apply database migrations")?;
    }
    let chain = SorobanChain::new(&ChainSettings::new(&cfg.rpc_url, &cfg.network_passphrase, &cfg.contract_id))
        .context("invalid chain settings")?;
    let (gh, github_configured): (Arc<dyn GithubApi>, bool) = match &cfg.github {
        Some(g) => (Arc::new(GithubClient::new(GithubClientConfig::new(&g.app_id, &g.private_key)).context("GitHub App key")?), true),
        None => (Arc::new(NoGithub), false),
    };
    let relayer =
        cfg.relayer_secret.as_deref().map(|s| Relayer::new(chain.clone(), s).map(Arc::new)).transpose().context("RELAYER_SECRET")?;
    let ai_enabled = github_configured && cfg.anthropic_api_key.is_some();
    Ok(Runtime { cfg, db, chain, gh, github_configured, relayer, ai_enabled })
}

impl Runtime {
    pub fn tokens(&self) -> Tokens {
        Tokens { usdc: self.cfg.token_usdc.clone(), xlm: self.cfg.token_xlm.clone() }
    }

    fn scheduler(&self) -> Option<Arc<dyn ReviewScheduler>> {
        self.ai_enabled.then(|| Arc::new(JobReviewScheduler { db: self.db.clone() }) as Arc<dyn ReviewScheduler>)
    }

    pub fn payout_deps(&self) -> PayoutDeps {
        PayoutDeps {
            db: self.db.clone(),
            chain: Arc::new(self.chain.clone()) as Arc<dyn ChainReader>,
            notifier: Arc::new(GithubNotifier { db: self.db.clone(), gh: self.gh.clone() }),
            web_url: self.cfg.web_public_url.clone(),
        }
    }

    pub fn sync_deps(&self) -> SyncDeps {
        SyncDeps {
            db: self.db.clone(),
            gh: self.gh.clone(),
            web_url: self.cfg.web_public_url.clone(),
            tokens: self.tokens(),
            explorer_template: self.cfg.explorer_tx_template.clone(),
        }
    }

    pub fn indexer_deps(&self, start_ledger: Option<u32>) -> IndexerDeps {
        IndexerDeps {
            db: self.db.clone(),
            source: Arc::new(self.chain.clone()) as Arc<dyn EventSource>,
            chain: Arc::new(self.chain.clone()) as Arc<dyn ChainReader>,
            start_ledger: start_ledger.or(self.cfg.indexer_start_ledger),
            lookback: self.cfg.indexer_lookback_ledgers,
        }
    }

    pub fn dispatch_deps(&self) -> DispatchDeps {
        let unit = 10_000_000i128;
        DispatchDeps {
            db: self.db.clone(),
            commands: CommandDeps {
                db: self.db.clone(),
                gh: self.gh.clone(),
                web_url: self.cfg.web_public_url.clone(),
                tokens: TokenTable {
                    usdc: Some(self.cfg.token_usdc.clone()),
                    xlm: Some(self.cfg.token_xlm.clone()),
                    min: self.cfg.token_min * unit,
                    max: self.cfg.token_max * unit,
                },
                min_days: self.cfg.min_days,
                max_days: self.cfg.max_days,
                reviews: self.scheduler(),
                now: Utc::now,
            },
            pulls: PullDeps {
                db: self.db.clone(),
                payouts: self.payout_deps(),
                reviews: self.scheduler(),
                review_delay: Duration::from_secs(self.cfg.review_debounce_secs),
            },
        }
    }

    pub fn review_runner(&self) -> Option<Arc<dyn ReviewRunner>> {
        let key = self.cfg.anthropic_api_key.as_deref().filter(|_| self.ai_enabled)?;
        Some(Arc::new(AiRunner(ReviewDeps {
            db: self.db.clone(),
            gh: self.gh.clone(),
            model: Arc::new(AnthropicModel::new(key, &self.cfg.ai_model)),
            model_name: self.cfg.ai_model.clone(),
            max_per_pr_per_day: i64::from(self.cfg.ai_max_per_pr_per_day),
            max_per_repo_per_day: i64::from(self.cfg.ai_max_per_repo_per_day),
        })))
    }

    pub fn worker_deps(&self) -> Arc<WorkerDeps> {
        Arc::new(WorkerDeps {
            db: self.db.clone(),
            dispatch: self.dispatch_deps(),
            sync: self.sync_deps(),
            reviews: self.review_runner(),
            chain: Arc::new(self.chain.clone()),
            relayer: self.relayer.clone().map(|r| r as Arc<dyn ReleaseSigner>),
            alerts: Arc::new(AlertSender::new(self.cfg.alert_webhook_url.clone(), Duration::from_secs(3600))),
            min_relayer_xlm: self.cfg.relayer_min_xlm_alert,
            grace: Chrono::hours(72),
        })
    }

    pub fn api_state(&self) -> AppState {
        let c = &self.cfg;
        let login = c.github.as_ref().map(|g| {
            Arc::new(RealLogin { client_id: g.client_id.clone(), client: OAuthClient::new(&g.client_id, &g.client_secret) })
                as Arc<dyn escrow_api::login::GithubLogin>
        });
        AppState {
            db: self.db.clone(),
            cfg: Arc::new(ApiConfig {
                csrf_secret: c.csrf_secret.clone(),
                session_secret: c.session_secret.clone(),
                webhook_secret: c.webhook_secret.clone(),
                api_public_url: c.api_public_url.clone(),
                web_public_url: c.web_public_url.clone(),
                secure_cookies: c.secure_cookies(),
                admin_github_ids: c.admin_github_ids.clone(),
                trust_proxy: c.trust_proxy,
                metrics_token: c.metrics_token.clone(),
                explorer_tx_template: c.explorer_tx_template.clone(),
                tokens: self.tokens(),
                network: format!("{:?}", c.network).to_lowercase(),
            }),
            chain: Arc::new(self.chain.clone()) as Arc<dyn ChainReader>,
            txb: Arc::new(self.chain.clone()) as Arc<dyn TxBuilder>,
            login,
            payouts: self.payout_deps(),
            limiter: Arc::new(RateLimiter::new(Duration::from_secs(60))),
            metrics: Arc::new(Metrics::default()),
            now: Utc::now,
        }
    }
}
