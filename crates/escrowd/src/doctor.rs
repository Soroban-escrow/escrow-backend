//! `escrowd doctor`: every dependency, checked, with what to do about a problem.

use crate::runtime;
use anyhow::Result;
use escrow_chain::{ChainReader, ReleaseSigner};
use escrow_core::config::{Config, Network};
use escrow_db::webhooks;

enum S {
    Ok,
    Warn,
    Fail,
}

struct Report {
    rows: Vec<(S, String, String)>,
}

impl Report {
    fn add(&mut self, s: S, name: &str, detail: impl Into<String>) {
        self.rows.push((s, name.into(), detail.into()));
    }
}

pub async fn run(cfg: Config) -> Result<()> {
    let mut r = Report { rows: vec![] };
    r.add(S::Ok, "configuration", format!("valid ({:?}, contract {}…)", cfg.network, &cfg.contract_id[..8]));

    match runtime::connect(cfg.clone(), false).await {
        Err(e) => r.add(S::Fail, "database", format!("{e:#}. Check DATABASE_URL and that Postgres is running.")),
        Ok(rt) => {
            let migrated: Result<i64, _> =
                sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(&rt.db.pool).await;
            match migrated {
                Ok(n) if n > 0 => r.add(S::Ok, "database", format!("connected, {n} migration(s) applied")),
                _ => r.add(
                    S::Warn,
                    "database",
                    "connected, but no migrations applied yet. Run `escrowd migrate` (or start with AUTO_MIGRATE on).",
                ),
            }

            match rt.chain.latest_ledger().await {
                Err(e) => r.add(S::Fail, "stellar rpc", format!("{e}. Check STELLAR_RPC_URL.")),
                Ok(tip) => {
                    r.add(S::Ok, "stellar rpc", format!("reachable, ledger {tip}"));
                    match rt.chain.get_config().await {
                        Err(e) => r.add(
                            S::Fail,
                            "contract",
                            format!("cannot read the contract: {e}. Is BOUNTY_CONTRACT_ID right for this network?"),
                        ),
                        Ok(c) => {
                            let detail = format!(
                                "fee {:.2}%, refund grace {} h, arbiter {}…",
                                f64::from(c.fee_bps) / 100.0,
                                c.grace_secs / 3600,
                                &c.arbiter[..8]
                            );
                            r.add(
                                if c.paused { S::Warn } else { S::Ok },
                                "contract",
                                if c.paused { format!("{detail} — PAUSED: new funding is blocked") } else { detail },
                            );
                            match &rt.relayer {
                                None => r.add(
                                    S::Warn,
                                    "relayer",
                                    "no RELAYER_SECRET: automatic payouts are off; maintainers must sign every release",
                                ),
                                Some(rel) => {
                                    let (addr, bal) = (rel.address(), rel.balance_xlm().await);
                                    if addr != c.relayer {
                                        r.add(
                                            S::Fail,
                                            "relayer",
                                            format!(
                                                "{addr} is not the relayer registered in the contract ({}). Payouts would be rejected.",
                                                c.relayer
                                            ),
                                        );
                                    } else {
                                        match bal {
                                            Ok(b) if b >= cfg.relayer_min_xlm_alert => {
                                                r.add(S::Ok, "relayer", format!("{}…, balance {b:.2} XLM", &addr[..8]))
                                            }
                                            Ok(b) => r.add(
                                                S::Warn,
                                                "relayer",
                                                format!(
                                                    "{}…, balance {b:.2} XLM is below the alert level ({}): top it up",
                                                    &addr[..8],
                                                    cfg.relayer_min_xlm_alert
                                                ),
                                            ),
                                            Err(e) => r.add(
                                                S::Fail,
                                                "relayer",
                                                format!("registered, but its account cannot be read: {e}. Is it funded?"),
                                            ),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Ok(Some(cur)) = webhooks::get_cursor(&rt.db.pool, escrow_engine::indexer::CURSOR_ID).await {
                        let lag = i64::from(tip) - i64::from(cur.ledger);
                        r.add(if lag <= 60 { S::Ok } else { S::Warn }, "indexer", format!("at ledger {} ({lag} behind)", cur.ledger));
                    } else {
                        r.add(S::Warn, "indexer", "has not run yet (no cursor). It starts with the worker.");
                    }
                }
            }

            if rt.github_configured {
                match rt.gh.list_installations().await {
                    Ok(i) => r.add(S::Ok, "github app", format!("authenticated; installed on {} account(s)", i.len())),
                    Err(e) => r.add(S::Fail, "github app", format!("{e}. Check GITHUB_APP_ID and the private key.")),
                }
            } else {
                r.add(S::Warn, "github app", "not configured: no webhooks, comments or sign-in. See docs/GITHUB_APP.md.");
            }
        }
    }

    for (name, id) in [("USDC token", &cfg.token_usdc), ("XLM token", &cfg.token_xlm)] {
        if id.is_empty() {
            r.add(
                S::Warn,
                name,
                format!(
                    "TOKEN_{}_CONTRACT_ID is not set: `/bounty … {}` is refused",
                    name.split(' ').next().unwrap_or(""),
                    name.split(' ').next().unwrap_or("")
                ),
            );
        } else {
            r.add(S::Ok, name, id[..id.len().min(8)].to_string() + "…");
        }
    }
    r.add(
        if cfg.anthropic_api_key.is_some() { S::Ok } else { S::Warn },
        "ai review",
        if cfg.anthropic_api_key.is_some() {
            format!("enabled ({})", cfg.ai_model)
        } else {
            "off: no ANTHROPIC_API_KEY (reviews are optional and never affect payouts)".into()
        },
    );
    if cfg.network == Network::Mainnet && !cfg.api_public_url.starts_with("https://") {
        r.add(S::Fail, "urls", "mainnet needs an https API_PUBLIC_URL (cookies must be Secure)");
    } else if !cfg.api_public_url.starts_with("https://") && !cfg.api_public_url.contains("localhost") {
        r.add(S::Warn, "urls", "API_PUBLIC_URL is plain http on a public host: sessions are not protected in transit");
    } else {
        r.add(S::Ok, "urls", format!("{} / {}", cfg.api_public_url, cfg.web_public_url));
    }
    if cfg.admin_github_ids.is_empty() {
        r.add(S::Warn, "admins", "ADMIN_GITHUB_IDS is empty: nobody can use the dispute/admin tools");
    }

    let mut failed = 0;
    for (s, name, detail) in &r.rows {
        let mark = match s {
            S::Ok => "  ok  ",
            S::Warn => " warn ",
            S::Fail => {
                failed += 1;
                " FAIL "
            }
        };
        println!("[{mark}] {name:<14} {detail}");
    }
    if failed > 0 {
        anyhow::bail!("{failed} check(s) failed");
    }
    Ok(())
}
