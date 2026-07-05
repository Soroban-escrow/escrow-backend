//! Operations: health, alerts.

use crate::indexer::CURSOR_ID;
use chrono::{DateTime, Duration, Utc};
use escrow_chain::ChainReader;
use escrow_db::{Db, disputes, jobs, payouts, webhooks};
use serde::Serialize;
use std::{collections::HashMap, sync::Mutex, time::Instant};

/// Ledgers behind the tip that we still call healthy (60 is about 5 minutes).
pub const MAX_LAG_LEDGERS: i64 = 60;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub ok: bool,
    pub db: bool,
    pub rpc: bool,
    pub indexer_lag_ledgers: Option<i64>,
    pub dead_jobs: Option<i64>,
}

/// Dependency checks for `/health`. 503 when the database or RPC is unreachable or the indexer is far behind.
/// Dead jobs are reported but do not fail health (alerts cover them).
pub async fn health_report(db: &Db, chain: &dyn ChainReader) -> Health {
    let db_ok = db.ping().await.is_ok();
    let tip = chain.latest_ledger().await.ok();
    let mut lag = None;
    let mut dead = None;
    if db_ok {
        if let (Some(tip), Ok(Some(c))) = (tip, webhooks::get_cursor(&db.pool, CURSOR_ID).await) {
            lag = Some((i64::from(tip) - i64::from(c.ledger)).max(0));
        }
        dead = jobs::count_dead(&db.pool).await.ok();
    }
    let lag_ok = lag.is_none_or(|l| l <= MAX_LAG_LEDGERS);
    Health { ok: db_ok && tip.is_some() && lag_ok, db: db_ok, rpc: tip.is_some(), indexer_lag_ledgers: lag, dead_jobs: dead }
}

pub struct AlertInputs {
    pub relayer_balance_xlm: Option<f64>,
    pub min_relayer_xlm: f64,
    pub latest_ledger: Option<u32>,
    pub now: DateTime<Utc>,
}

/// The human-readable alerts that apply right now.
pub async fn collect_alerts(db: &Db, a: &AlertInputs) -> Result<Vec<String>, sqlx::Error> {
    let mut out = vec![];
    if let Some(b) = a.relayer_balance_xlm
        && b < a.min_relayer_xlm
    {
        out.push(format!("Relayer balance low: {b:.2} XLM (alert below {})", a.min_relayer_xlm));
    }
    let failed = payouts::count_by_status(&db.pool, &["FAILED"]).await?;
    if failed > 0 {
        out.push(format!("{failed} payout(s) FAILED and need attention"));
    }
    if let (Some(tip), Some(c)) = (a.latest_ledger, webhooks::get_cursor(&db.pool, CURSOR_ID).await?) {
        let lag = i64::from(tip) - i64::from(c.ledger);
        if lag > MAX_LAG_LEDGERS {
            out.push(format!("Indexer is {lag} ledgers behind"));
        }
    }
    let stale = disputes::count_open_older_than(&db.pool, a.now - Duration::hours(36)).await?;
    if stale > 0 {
        out.push(format!("{stale} dispute(s) older than 36 h"));
    }
    // merged-but-unpaid PRs whose refund window opens within 72 h: the arbiter may need to act
    let risky: Vec<(i64, String)> = sqlx::query_as(
        "SELECT b.id, p.status FROM payouts p JOIN bounties b ON b.id = p.bounty_id
         WHERE p.status IN ('PENDING_WALLET','AWAITING_MAINTAINER','FAILED') AND b.status = 'OPEN' AND b.deadline < $1 ORDER BY b.id",
    )
    .bind(a.now + Duration::hours(72))
    .fetch_all(&db.pool)
    .await?;
    for (id, status) in risky {
        out.push(format!("Bounty {id} has a merged PR ({status}) and its refund window is near"));
    }
    let dead = jobs::count_dead(&db.pool).await?;
    if dead > 0 {
        out.push(format!("{dead} background job(s) gave up and are dead (see the jobs table)"));
    }
    Ok(out)
}

/// Sends alerts to a webhook, each distinct message at most once per cooldown.
pub struct AlertSender {
    url: Option<String>,
    http: reqwest::Client,
    cooldown: std::time::Duration,
    sent: Mutex<HashMap<String, Instant>>,
}

impl AlertSender {
    pub fn new(url: Option<String>, cooldown: std::time::Duration) -> Self {
        Self {
            url,
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().expect("client builds"),
            cooldown,
            sent: Mutex::default(),
        }
    }

    /// Returns the messages actually sent (those not in cooldown).
    pub async fn send(&self, messages: &[String], now: Instant) -> Vec<String> {
        let fresh: Vec<String> = {
            let mut sent = self.sent.lock().unwrap();
            messages
                .iter()
                .filter(|m| {
                    let due = sent.get(*m).is_none_or(|t| now.duration_since(*t) >= self.cooldown);
                    if due {
                        sent.insert((*m).clone(), now);
                    }
                    due
                })
                .cloned()
                .collect()
        };
        for m in &fresh {
            tracing::error!(alert = %m, "ALERT");
            if let Some(url) = &self.url {
                let _ = self.http.post(url).json(&serde_json::json!({ "text": format!("SkillEscrow: {m}") })).send().await;
            }
        }
        fresh
    }
}
