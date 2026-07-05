//! Typed configuration. This is the only place that knows the environment's shape; it validates
//! everything up front and reports every problem at once, so a bad deploy fails at start, not at 3 a.m.
//! Variable names match the TypeScript API, so an existing `.env` keeps working.

use regex::Regex;
use std::{collections::HashMap, fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Testnet,
    Mainnet,
    Local,
}

impl FromStr for Network {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "testnet" => Ok(Self::Testnet),
            "mainnet" => Ok(Self::Mainnet),
            "local" => Ok(Self::Local),
            _ => Err(()),
        }
    }
}

#[derive(Clone)]
pub struct GithubAppConfig {
    pub app_id: String,
    pub slug: String,
    pub private_key: String,
    pub client_id: String,
    pub client_secret: String,
}

/// Secrets never appear in `Debug` output, so a stray `{:?}` cannot leak them into logs.
impl fmt::Debug for GithubAppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubAppConfig").field("app_id", &self.app_id).field("slug", &self.slug).finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Config {
    pub log_level: String,
    pub port: u16,
    pub database_url: String,
    pub network: Network,
    pub rpc_url: String,
    pub network_passphrase: String,
    pub contract_id: String,
    pub api_public_url: String,
    pub web_public_url: String,
    pub session_secret: String,
    pub csrf_secret: String,
    pub webhook_secret: String,
    pub github: Option<GithubAppConfig>,
    pub admin_github_ids: Vec<i64>,
    pub anthropic_api_key: Option<String>,
    pub ai_model: String,
    pub ai_max_per_pr_per_day: u32,
    pub ai_max_per_repo_per_day: u32,
    pub explorer_tx_template: String,
    pub relayer_secret: Option<String>,
    pub relayer_min_xlm_alert: f64,
    pub alert_webhook_url: Option<String>,
    pub token_usdc: String,
    pub token_xlm: String,
    /// per-bounty limits in whole tokens (the contract enforces its own, authoritative ones)
    pub token_min: i128,
    pub token_max: i128,
    pub min_days: u32,
    pub max_days: u32,
    pub indexer_interval_secs: u64,
    pub indexer_lookback_ledgers: u32,
    pub review_debounce_secs: u64,
    /// believe `X-Forwarded-For` (only when behind a reverse proxy you control)
    pub trust_proxy: bool,
    /// when set, `/metrics` requires `Authorization: Bearer <token>`
    pub metrics_token: Option<String>,
    pub indexer_start_ledger: Option<u32>,
    pub db_max_connections: u32,
    pub worker_concurrency: usize,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("network", &self.network)
            .field("rpc_url", &self.rpc_url)
            .field("contract_id", &self.contract_id)
            .field("port", &self.port)
            .field("github", &self.github)
            .field("relayer", &self.relayer_secret.as_ref().map(|_| "<set>"))
            .field("ai", &self.anthropic_api_key.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Invalid environment:")?;
        for l in &self.0 {
            writeln!(f, "  {l}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ConfigError {}

struct Reader<'a> {
    env: &'a HashMap<String, String>,
    errors: Vec<String>,
}

impl<'a> Reader<'a> {
    fn raw(&self, k: &str) -> Option<&'a str> {
        self.env.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty())
    }
    fn required(&mut self, k: &str) -> String {
        match self.raw(k) {
            Some(v) => v.to_string(),
            None => {
                self.errors.push(format!("{k}: required"));
                String::new()
            }
        }
    }
    fn string(&self, k: &str, default: &str) -> String {
        self.raw(k).unwrap_or(default).to_string()
    }
    fn optional(&self, k: &str) -> Option<String> {
        self.raw(k).map(str::to_string)
    }
    fn number<T: FromStr>(&mut self, k: &str, default: T) -> T {
        match self.raw(k) {
            None => default,
            Some(v) => v.parse().unwrap_or_else(|_| {
                self.errors.push(format!("{k}: `{v}` is not a valid number"));
                default
            }),
        }
    }
    fn url(&mut self, k: &str, default: Option<&str>) -> String {
        let v = match (self.raw(k), default) {
            (Some(v), _) => v.to_string(),
            (None, Some(d)) => d.to_string(),
            (None, None) => {
                self.errors.push(format!("{k}: required"));
                return String::new();
            }
        };
        if !(v.starts_with("http://") || v.starts_with("https://")) {
            self.errors.push(format!("{k}: must be an http(s) URL"));
        }
        v.trim_end_matches('/').to_string()
    }
    fn secret_min(&mut self, k: &str, min: usize) -> String {
        let v = self.required(k);
        if !v.is_empty() && v.len() < min {
            self.errors.push(format!("{k}: must be at least {min} characters"));
        }
        v
    }
    fn matches(&mut self, k: &str, re: &Regex, what: &str) -> Option<String> {
        let v = self.raw(k)?;
        if re.is_match(v) {
            Some(v.to_string())
        } else {
            self.errors.push(format!("{k}: {what}"));
            None
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_map(&std::env::vars().collect())
    }

    pub fn from_map(env: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let mut r = Reader { env, errors: vec![] };
        let strkey_c = Regex::new(r"^C[A-Z2-7]{55}$").unwrap();
        let strkey_s = Regex::new(r"^S[A-Z2-7]{55}$").unwrap();
        let digits_list = Regex::new(r"^\d+(,\d+)*$").unwrap();
        let digits = Regex::new(r"^\d+$").unwrap();

        let network = match r.string("STELLAR_NETWORK", "testnet").parse::<Network>() {
            Ok(n) => n,
            Err(()) => {
                r.errors.push("STELLAR_NETWORK: must be testnet, mainnet or local".into());
                Network::Testnet
            }
        };
        let contract_id = r.required("BOUNTY_CONTRACT_ID");
        if !contract_id.is_empty() && !strkey_c.is_match(&contract_id) {
            r.errors.push("BOUNTY_CONTRACT_ID: must be a Soroban contract id (C…, 56 characters)".into());
        }

        let app_id = r.matches("GITHUB_APP_ID", &digits, "must be numeric");
        let slug = r.optional("GITHUB_APP_SLUG");
        let key = r.optional("GITHUB_APP_PRIVATE_KEY");
        if let Some(k) = &key
            && !k.contains("PRIVATE KEY")
        {
            r.errors.push("GITHUB_APP_PRIVATE_KEY: must be a PEM private key".into());
        }
        let client_id = r.optional("GITHUB_CLIENT_ID");
        let client_secret = r.optional("GITHUB_CLIENT_SECRET");
        let set = [app_id.is_some(), slug.is_some(), key.is_some(), client_id.is_some(), client_secret.is_some()];
        let github = match set.iter().filter(|x| **x).count() {
            0 => None,
            5 => Some(GithubAppConfig {
                app_id: app_id.unwrap(),
                slug: slug.unwrap(),
                private_key: key.unwrap(),
                client_id: client_id.unwrap(),
                client_secret: client_secret.unwrap(),
            }),
            _ => {
                r.errors.push("GITHUB_APP_ID: set all five GitHub App variables (APP_ID, APP_SLUG, APP_PRIVATE_KEY, CLIENT_ID, CLIENT_SECRET) or none".into());
                None
            }
        };

        let relayer_secret = r.matches("RELAYER_SECRET", &strkey_s, "must be a Stellar secret key (S…, 56 characters)");
        if network == Network::Mainnet && relayer_secret.is_some() {
            r.errors.push("RELAYER_SECRET: must not be used on mainnet; use a KMS-backed signer".into());
        }

        let admin_github_ids = r
            .matches("ADMIN_GITHUB_IDS", &digits_list, "comma-separated numeric GitHub ids")
            .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
            .unwrap_or_default();
        let anthropic_api_key = r.optional("ANTHROPIC_API_KEY");
        if anthropic_api_key.as_deref().is_some_and(|k| k.len() < 10) {
            r.errors.push("ANTHROPIC_API_KEY: looks too short".into());
        }

        // Testnet has a well-known public RPC. Mainnet has none we would vouch for: choose a provider on purpose.
        let default_rpc = match network {
            Network::Mainnet => None,
            _ => Some("https://soroban-testnet.stellar.org"),
        };
        let default_pass = match network {
            Network::Mainnet => "Public Global Stellar Network ; September 2015",
            Network::Local => "Standalone Network ; February 2017",
            Network::Testnet => "Test SDF Network ; September 2015",
        };
        let default_explorer = match network {
            Network::Mainnet => "https://stellar.expert/explorer/public/tx/{hash}",
            _ => "https://stellar.expert/explorer/testnet/tx/{hash}",
        };

        let token_min = r.number("TOKEN_MIN_AMOUNT", 1i128);
        let token_max = r.number("TOKEN_MAX_AMOUNT", 500i128);
        if token_min < 1 || token_max < token_min {
            r.errors.push("TOKEN_MIN_AMOUNT/TOKEN_MAX_AMOUNT: need 1 <= min <= max".into());
        }
        let (min_days, max_days) = (r.number("MIN_DAYS", 1u32), r.number("MAX_DAYS", 90u32));
        if min_days < 1 || max_days < min_days {
            r.errors.push("MIN_DAYS/MAX_DAYS: need 1 <= min <= max".into());
        }

        let cfg = Config {
            log_level: r.string("LOG_LEVEL", "info"),
            port: r.number("PORT", 3001u16),
            database_url: r.required("DATABASE_URL"),
            network,
            rpc_url: r.url("STELLAR_RPC_URL", default_rpc),
            network_passphrase: r.string("STELLAR_NETWORK_PASSPHRASE", default_pass),
            contract_id,
            api_public_url: r.url("API_PUBLIC_URL", Some("http://localhost:3001")),
            web_public_url: r.url("WEB_PUBLIC_URL", Some("http://localhost:3000")),
            session_secret: r.secret_min("SESSION_SECRET", 32),
            csrf_secret: r.secret_min("CSRF_SECRET", 32),
            webhook_secret: r.secret_min("GITHUB_WEBHOOK_SECRET", 8),
            github,
            admin_github_ids,
            anthropic_api_key,
            ai_model: r.string("AI_MODEL", "claude-sonnet-5-5"),
            ai_max_per_pr_per_day: r.number("AI_MAX_REVIEWS_PER_PR_PER_DAY", 5u32),
            ai_max_per_repo_per_day: r.number("AI_MAX_REVIEWS_PER_REPO_PER_DAY", 50u32),
            explorer_tx_template: r.string("EXPLORER_TX_URL_TEMPLATE", default_explorer),
            relayer_secret,
            relayer_min_xlm_alert: r.number("RELAYER_MIN_XLM_ALERT", 50.0f64),
            alert_webhook_url: r.optional("ALERT_WEBHOOK_URL"),
            token_usdc: r.string("TOKEN_USDC_CONTRACT_ID", ""),
            token_xlm: r.string("TOKEN_XLM_CONTRACT_ID", ""),
            token_min,
            token_max,
            min_days,
            max_days,
            indexer_interval_secs: r.number("INDEXER_INTERVAL_SECS", 5u64),
            indexer_lookback_ledgers: r.number("INDEXER_LOOKBACK_LEDGERS", 1000u32),
            review_debounce_secs: r.number("REVIEW_DEBOUNCE_SECS", 300u64),
            trust_proxy: matches!(r.raw("TRUST_PROXY"), Some("1" | "true")),
            metrics_token: r.optional("METRICS_TOKEN"),
            indexer_start_ledger: r.raw("INDEXER_START_LEDGER").and_then(|v| v.parse().ok()),
            db_max_connections: r.number("DB_MAX_CONNECTIONS", 10u32),
            worker_concurrency: r.number("WORKER_CONCURRENCY", 4usize),
        };
        if r.errors.is_empty() { Ok(cfg) } else { Err(ConfigError(r.errors)) }
    }

    /// Whether cookies should carry the `Secure` flag (true when the public API URL is https).
    pub fn secure_cookies(&self) -> bool {
        self.api_public_url.starts_with("https://")
    }

    pub fn symbol_for(&self, token: &str) -> String {
        if !self.token_usdc.is_empty() && token == self.token_usdc {
            "USDC".into()
        } else if !self.token_xlm.is_empty() && token == self.token_xlm {
            "XLM".into()
        } else {
            format!("{}…", &token[..token.len().min(4)])
        }
    }

    pub fn explorer_tx(&self, hash: &str) -> String {
        self.explorer_tx_template.replace("{hash}", hash)
    }

    pub fn is_admin(&self, github_id: i64) -> bool {
        self.admin_github_ids.contains(&github_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V";

    fn base() -> HashMap<String, String> {
        [
            ("DATABASE_URL", "postgres://u:p@localhost/db"),
            ("BOUNTY_CONTRACT_ID", CONTRACT),
            ("SESSION_SECRET", "s".repeat(32).leak()),
            ("CSRF_SECRET", "c".repeat(32).leak()),
            ("GITHUB_WEBHOOK_SECRET", "whsec-1234"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn with(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut m = base();
        m.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        m
    }

    #[test]
    fn minimal_environment_gets_sane_defaults() {
        let c = Config::from_map(&base()).unwrap();
        assert_eq!(c.port, 3001);
        assert_eq!(c.network, Network::Testnet);
        assert_eq!(c.rpc_url, "https://soroban-testnet.stellar.org");
        assert_eq!(c.network_passphrase, "Test SDF Network ; September 2015");
        assert!(c.github.is_none() && c.relayer_secret.is_none());
        assert_eq!((c.min_days, c.max_days), (1, 90));
        assert!(!c.secure_cookies());
    }

    #[test]
    fn reports_every_problem_at_once() {
        let e = Config::from_map(&HashMap::new()).unwrap_err().to_string();
        for needle in ["DATABASE_URL", "BOUNTY_CONTRACT_ID", "SESSION_SECRET", "CSRF_SECRET", "GITHUB_WEBHOOK_SECRET"] {
            assert!(e.contains(needle), "missing {needle} in:\n{e}");
        }
    }

    #[test]
    fn github_variables_are_all_or_none() {
        let e = Config::from_map(&with(&[("GITHUB_APP_ID", "123")])).unwrap_err().to_string();
        assert!(e.contains("all five GitHub App variables"));
        let full = with(&[
            ("GITHUB_APP_ID", "123"),
            ("GITHUB_APP_SLUG", "skillescrow"),
            ("GITHUB_APP_PRIVATE_KEY", &format!("-----BEGIN RSA {0} KEY-----\nabc\n-----END RSA {0} KEY-----", "PRIVATE")),
            ("GITHUB_CLIENT_ID", "Iv1.abc"),
            ("GITHUB_CLIENT_SECRET", "secret"),
        ]);
        let c = Config::from_map(&full).unwrap();
        assert_eq!(c.github.as_ref().unwrap().slug, "skillescrow");
        // secrets stay out of Debug output
        assert!(!format!("{c:?}").contains("abc") && !format!("{c:?}").contains("secret"));
    }

    #[test]
    fn mainnet_makes_you_choose_an_rpc_provider() {
        let e = Config::from_map(&with(&[("STELLAR_NETWORK", "mainnet")])).unwrap_err().to_string();
        assert!(e.contains("STELLAR_RPC_URL: required"), "{e}");
        let c = Config::from_map(&with(&[("STELLAR_NETWORK", "mainnet"), ("STELLAR_RPC_URL", "https://rpc.example/")])).unwrap();
        assert_eq!(c.rpc_url, "https://rpc.example");
        assert_eq!(c.network_passphrase, "Public Global Stellar Network ; September 2015");
        assert!(c.explorer_tx_template.contains("/explorer/public/"));
    }

    #[test]
    fn relayer_secret_is_refused_on_mainnet() {
        let sk = format!("S{}", "A".repeat(55));
        assert!(Config::from_map(&with(&[("RELAYER_SECRET", &sk)])).is_ok());
        let e = Config::from_map(&with(&[("RELAYER_SECRET", &sk), ("STELLAR_NETWORK", "mainnet")])).unwrap_err().to_string();
        assert!(e.contains("must not be used on mainnet"));
    }

    #[test]
    fn rejects_malformed_values() {
        assert!(Config::from_map(&with(&[("BOUNTY_CONTRACT_ID", "GABC")])).is_err());
        assert!(Config::from_map(&with(&[("SESSION_SECRET", "short")])).is_err());
        assert!(Config::from_map(&with(&[("PORT", "abc")])).is_err());
        assert!(Config::from_map(&with(&[("STELLAR_RPC_URL", "ftp://x")])).is_err());
        assert!(Config::from_map(&with(&[("STELLAR_NETWORK", "devnet")])).is_err());
        assert!(Config::from_map(&with(&[("ADMIN_GITHUB_IDS", "1,x")])).is_err());
        assert!(Config::from_map(&with(&[("TOKEN_MIN_AMOUNT", "10"), ("TOKEN_MAX_AMOUNT", "5")])).is_err());
    }

    #[test]
    fn helpers() {
        let c = Config::from_map(&with(&[
            ("TOKEN_XLM_CONTRACT_ID", "CXLMXLMX"),
            ("ADMIN_GITHUB_IDS", "7,9"),
            ("API_PUBLIC_URL", "https://api.example/"),
        ]))
        .unwrap();
        assert_eq!(c.symbol_for("CXLMXLMX"), "XLM");
        assert_eq!(c.symbol_for("CABCDEF"), "CABC…");
        assert!(c.is_admin(9) && !c.is_admin(8));
        assert!(c.secure_cookies());
        assert_eq!(c.api_public_url, "https://api.example");
        assert_eq!(c.explorer_tx("abc"), "https://stellar.expert/explorer/testnet/tx/abc");
    }
}
