//! `escrowd doctor`: every dependency, checked, with what to do about a problem.

use crate::runtime;
use anyhow::Result;
use escrow_chain::{ChainReader, ReleaseSigner};
use escrow_core::config::{Config, Network};
use escrow_db::webhooks;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Ok => "  ok  ",
            Self::Warn => " warn ",
            Self::Fail => " FAIL ",
        }
    }
}

struct Check {
    status: Status,
    name: String,
    detail: String,
    fix: Option<String>,
}

struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn ok(&mut self, name: &str, detail: impl Into<String>) {
        self.checks.push(Check { status: Status::Ok, name: name.into(), detail: detail.into(), fix: None });
    }

    fn add(&mut self, status: Status, name: &str, detail: impl Into<String>, fix: Option<impl Into<String>>) {
        self.checks.push(Check { status, name: name.into(), detail: detail.into(), fix: fix.map(Into::into) });
    }

    fn failed_count(&self) -> usize {
        self.checks.iter().filter(|c| c.status == Status::Fail).count()
    }

    fn to_json_value(&self) -> Value {
        Value::Array(
            self.checks
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "status": c.status.as_str(),
                        "detail": c.detail,
                        "fix": c.fix,
                    })
                })
                .collect(),
        )
    }

    fn format_human(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            let mark = c.status.mark();
            match &c.fix {
                Some(fix) => out.push_str(&format!("[{mark}] {:<14} {} — {fix}\n", c.name, c.detail)),
                None => out.push_str(&format!("[{mark}] {:<14} {}\n", c.name, c.detail)),
            }
        }
        out
    }

    fn format_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.to_json_value())?)
    }
}

pub async fn run(cfg: Config, as_json: bool) -> Result<()> {
    let mut r = Report::new();
    r.ok("configuration", format!("valid ({:?}, contract {}…)", cfg.network, &cfg.contract_id[..8]));

    match runtime::connect(cfg.clone(), false).await {
        Err(e) => r.add(Status::Fail, "database", format!("{e:#}"), Some("Check DATABASE_URL and that Postgres is running.")),
        Ok(rt) => {
            let migrated: Result<i64, _> =
                sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(&rt.db.pool).await;
            match migrated {
                Ok(n) if n > 0 => r.ok("database", format!("connected, {n} migration(s) applied")),
                _ => r.add(
                    Status::Warn,
                    "database",
                    "connected, but no migrations applied yet",
                    Some("Run `escrowd migrate` (or start with AUTO_MIGRATE on)."),
                ),
            }

            match rt.chain.latest_ledger().await {
                Err(e) => r.add(Status::Fail, "stellar rpc", format!("{e}"), Some("Check STELLAR_RPC_URL.")),
                Ok(tip) => {
                    r.ok("stellar rpc", format!("reachable, ledger {tip}"));
                    match rt.chain.get_config().await {
                        Err(e) => r.add(
                            Status::Fail,
                            "contract",
                            format!("cannot read the contract: {e}"),
                            Some("Check that BOUNTY_CONTRACT_ID is right for this network."),
                        ),
                        Ok(c) => {
                            let detail = format!(
                                "fee {:.2}%, refund grace {} h, arbiter {}…",
                                f64::from(c.fee_bps) / 100.0,
                                c.grace_secs / 3600,
                                &c.arbiter[..8]
                            );
                            if c.paused {
                                r.add(
                                    Status::Warn,
                                    "contract",
                                    format!("{detail} — PAUSED: new funding is blocked"),
                                    Some("Unpause the contract to resume new funding."),
                                );
                            } else {
                                r.ok("contract", detail);
                            }
                            match &rt.relayer {
                                None => r.add(
                                    Status::Warn,
                                    "relayer",
                                    "no RELAYER_SECRET: automatic payouts are off; maintainers must sign every release",
                                    Some("Set RELAYER_SECRET to enable automatic payouts."),
                                ),
                                Some(rel) => {
                                    let (addr, bal) = (rel.address(), rel.balance_xlm().await);
                                    if addr != c.relayer {
                                        r.add(
                                            Status::Fail,
                                            "relayer",
                                            format!(
                                                "{addr} is not the relayer registered in the contract ({}). Payouts would be rejected.",
                                                c.relayer
                                            ),
                                            Some("Update RELAYER_SECRET or register this address in the contract."),
                                        );
                                    } else {
                                        match bal {
                                            Ok(b) if b >= cfg.relayer_min_xlm_alert => {
                                                r.ok("relayer", format!("{}…, balance {b:.2} XLM", &addr[..8]))
                                            }
                                            Ok(b) => r.add(
                                                Status::Warn,
                                                "relayer",
                                                format!(
                                                    "{}…, balance {b:.2} XLM is below the alert level ({})",
                                                    &addr[..8],
                                                    cfg.relayer_min_xlm_alert
                                                ),
                                                Some("Top up the relayer account with XLM."),
                                            ),
                                            Err(e) => r.add(
                                                Status::Fail,
                                                "relayer",
                                                format!("registered, but its account cannot be read: {e}"),
                                                Some("Fund the relayer account on Stellar."),
                                            ),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Ok(Some(cur)) = webhooks::get_cursor(&rt.db.pool, escrow_engine::indexer::CURSOR_ID).await {
                        let lag = i64::from(tip) - i64::from(cur.ledger);
                        if lag <= 60 {
                            r.ok("indexer", format!("at ledger {} ({lag} behind)", cur.ledger));
                        } else {
                            r.add(
                                Status::Warn,
                                "indexer",
                                format!("at ledger {} ({lag} behind)", cur.ledger),
                                Some("Check that the worker is running and Stellar RPC is healthy."),
                            );
                        }
                    } else {
                        r.add(
                            Status::Warn,
                            "indexer",
                            "has not run yet (no cursor)",
                            Some("Start the worker (`escrowd worker` or `escrowd all`)."),
                        );
                    }
                }
            }

            if rt.github_configured {
                match rt.gh.list_installations().await {
                    Ok(i) => r.ok("github app", format!("authenticated; installed on {} account(s)", i.len())),
                    Err(e) => r.add(Status::Fail, "github app", format!("{e}"), Some("Check GITHUB_APP_ID and the private key.")),
                }
            } else {
                let waiting = match escrow_engine::jobs::count_waiting_for_github(&rt.db.pool).await {
                    Ok(n) => format!("; {n} job(s) waiting for it"),
                    Err(_) => String::new(),
                };
                r.add(
                    Status::Warn,
                    "github app",
                    format!("not configured: no webhooks, comments or sign-in{waiting}"),
                    Some("See docs/GITHUB_APP.md."),
                );
            }
        }
    }

    for (name, id) in [("USDC token", &cfg.token_usdc), ("XLM token", &cfg.token_xlm)] {
        if id.is_empty() {
            let sym = name.split(' ').next().unwrap_or("");
            r.add(
                Status::Warn,
                name,
                format!("TOKEN_{sym}_CONTRACT_ID is not set: `/bounty … {sym}` is refused"),
                Some(format!("Set TOKEN_{sym}_CONTRACT_ID.")),
            );
        } else {
            r.ok(name, id[..id.len().min(8)].to_string() + "…");
        }
    }
    if cfg.anthropic_api_key.is_some() {
        r.ok("ai review", format!("enabled ({})", cfg.ai_model));
    } else {
        r.add(
            Status::Warn,
            "ai review",
            "off: no ANTHROPIC_API_KEY (reviews are optional and never affect payouts)",
            Some("Set ANTHROPIC_API_KEY to enable optional AI reviews."),
        );
    }
    if cfg.network == Network::Mainnet && !cfg.api_public_url.starts_with("https://") {
        r.add(
            Status::Fail,
            "urls",
            "mainnet needs an https API_PUBLIC_URL (cookies must be Secure)",
            Some("Set API_PUBLIC_URL to an https:// URL."),
        );
    } else if !cfg.api_public_url.starts_with("https://") && !cfg.api_public_url.contains("localhost") {
        r.add(
            Status::Warn,
            "urls",
            "API_PUBLIC_URL is plain http on a public host: sessions are not protected in transit",
            Some("Serve the API over https:// and update API_PUBLIC_URL."),
        );
    } else {
        r.ok("urls", format!("{} / {}", cfg.api_public_url, cfg.web_public_url));
    }
    if cfg.metrics_token.is_none() {
        r.add(
            Status::Warn,
            "metrics",
            "METRICS_TOKEN is unset: /metrics is served without authentication",
            Some("Set METRICS_TOKEN; on mainnet it is required."),
        );
    }
    if cfg.admin_github_ids.is_empty() {
        r.add(
            Status::Warn,
            "admins",
            "ADMIN_GITHUB_IDS is empty: nobody can use the dispute/admin tools",
            Some("Set ADMIN_GITHUB_IDS to at least one GitHub user ID."),
        );
    }

    if as_json {
        println!("{}", r.format_json()?);
    } else {
        print!("{}", r.format_human());
    }
    let failed = r.failed_count();
    if failed > 0 {
        anyhow::bail!("{failed} check(s) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_has_one_object_per_check_with_expected_keys() {
        let mut r = Report::new();
        r.ok("configuration", "valid (Testnet, contract CABC1234…)");
        r.add(Status::Warn, "admins", "ADMIN_GITHUB_IDS is empty", Some("Set ADMIN_GITHUB_IDS."));
        r.add(Status::Fail, "database", "connection refused", Some("Check DATABASE_URL."));

        let raw = r.format_json().unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed.as_array().expect("doctor --json must output a JSON array");
        assert_eq!(arr.len(), 3);

        for item in arr {
            let obj = item.as_object().expect("each check must be a JSON object");
            assert_eq!(obj.len(), 4);
            assert!(obj.contains_key("name"));
            assert!(obj.contains_key("status"));
            assert!(obj.contains_key("detail"));
            assert!(obj.contains_key("fix"));
        }

        assert_eq!(
            arr[0],
            json!({ "name": "configuration", "status": "ok", "detail": "valid (Testnet, contract CABC1234…)", "fix": null })
        );
        assert_eq!(
            arr[1],
            json!({ "name": "admins", "status": "warn", "detail": "ADMIN_GITHUB_IDS is empty", "fix": "Set ADMIN_GITHUB_IDS." })
        );
        assert_eq!(arr[2], json!({ "name": "database", "status": "fail", "detail": "connection refused", "fix": "Check DATABASE_URL." }));
        assert_eq!(r.failed_count(), 1);
    }
}
