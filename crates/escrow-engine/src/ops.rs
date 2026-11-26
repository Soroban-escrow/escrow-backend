//! Operations: health, alerts.

use crate::indexer::CURSOR_ID;
use chrono::{DateTime, Duration, Utc};
use escrow_chain::ChainReader;
use escrow_core::PayoutStatus;
use escrow_db::{Db, audit, disputes, jobs, payouts, webhooks};
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

/// Dependency checks for `/health`. 503 when the database or RPC is unreachable, or (when
/// `check_indexer_lag` is true, i.e. processes running the indexer) when the indexer is far behind.
/// Dead jobs are reported but do not fail health (alerts cover them).
pub async fn health_report(db: &Db, chain: &dyn ChainReader, check_indexer_lag: bool) -> Health {
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
    let lag_ok = !check_indexer_lag || lag.is_none_or(|l| l <= MAX_LAG_LEDGERS);
    Health { ok: db_ok && tip.is_some() && lag_ok, db: db_ok, rpc: tip.is_some(), indexer_lag_ledgers: lag, dead_jobs: dead }
}

pub struct AlertInputs {
    /// `None` when no relayer is configured; otherwise `Some(Ok(balance))` or `Some(Err(reason))`.
    /// A failed read is surfaced as its own alert, so the window where the relayer has drained but
    /// the balance call is also failing (account missing, RPC down) still pages the operator.
    pub relayer_balance_xlm: Option<Result<f64, String>>,
    pub min_relayer_xlm: f64,
    pub latest_ledger: Option<u32>,
    pub now: DateTime<Utc>,
}

/// One alert. `key` (kind plus subject) identifies it for the cooldown; `text` is what people read and may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub key: String,
    pub text: String,
}

impl Alert {
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self { key: key.into(), text: text.into() }
    }
}

/// The alerts that apply right now.
pub async fn collect_alerts(db: &Db, a: &AlertInputs) -> Result<Vec<Alert>, sqlx::Error> {
    let mut out = vec![];
    match &a.relayer_balance_xlm {
        Some(Ok(b)) if *b < a.min_relayer_xlm => {
            out.push(Alert::new("relayer_balance", format!("Relayer balance low: {b:.2} XLM (alert below {})", a.min_relayer_xlm)));
        }
        Some(Err(reason)) => {
            // Keyed separately from `relayer_balance` so the cooldown on one does not swallow the
            // other, and so an operator can tell "balance low" from "balance unreadable" in the log.
            out.push(Alert::new("relayer_balance_read_failed", format!("Relayer balance could not be read: {reason}")));
        }
        _ => {}
    }
    let failed = payouts::count_by_status(&db.pool, &[PayoutStatus::Failed]).await?;
    if failed > 0 {
        out.push(Alert::new("payouts_failed", format!("{failed} payout(s) FAILED and need attention")));
    }
    if let (Some(tip), Some(c)) = (a.latest_ledger, webhooks::get_cursor(&db.pool, CURSOR_ID).await?) {
        let lag = i64::from(tip) - i64::from(c.ledger);
        if lag > MAX_LAG_LEDGERS {
            out.push(Alert::new("indexer_lag", format!("Indexer is {lag} ledgers behind")));
        }
    }
    let stale = disputes::count_open_older_than(&db.pool, a.now - Duration::hours(escrow_core::DISPUTE_ALERT_HOURS)).await?;
    if stale > 0 {
        out.push(Alert::new("disputes_stale", format!("{stale} dispute(s) older than {} h", escrow_core::DISPUTE_ALERT_HOURS)));
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
        out.push(Alert::new(
            format!("bounty_refund_window:{id}"),
            format!("Bounty {id} has a merged PR ({status}) and its refund window is near"),
        ));
    }
    let dead = jobs::count_dead(&db.pool).await?;
    if dead > 0 {
        out.push(Alert::new("jobs_dead", format!("{dead} background job(s) gave up and are dead (see the jobs table)")));
    }
    // Alert on any new anomalies in the last 24 hours
    let anomaly_count = audit::count_anomalies_24h(&db.pool).await?;
    if anomaly_count > 0 {
        let word = if anomaly_count == 1 { "anomaly" } else { "anomalies" };
        out.push(Alert::new("indexer_anomalies", format!("{anomaly_count} indexer {word} in the last 24h")));
    }
    // Bounties whose issue was transferred or deleted out from under them: recorded as `issue.*.alert`
    // audit entries by the webhook, surfaced here while the bounty is still live so it is not forgotten.
    let orphaned: Vec<(i64, String)> = sqlx::query_as(
        "SELECT b.id, a.action FROM bounties b
         JOIN audit_log a ON a.target = 'bounty:' || b.id
         WHERE b.status IN ('OPEN','DISPUTED') AND a.action IN ('issue.transferred.alert','issue.deleted.alert')
         ORDER BY b.id",
    )
    .fetch_all(&db.pool)
    .await?;
    for (id, action) in orphaned {
        let what = if action.starts_with("issue.deleted") { "deleted" } else { "transferred" };
        out.push(Alert::new(format!("bounty_orphaned:{id}"), format!("Bounty {id} is still active but its issue was {what}")));
    }
    Ok(out)
}

/// Sends alerts to a webhook, each alert key at most once per cooldown however its text changes.
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

    /// Returns the texts actually sent (alerts whose key is not in cooldown).
    pub async fn send(&self, alerts: &[Alert], now: Instant) -> Vec<String> {
        let fresh: Vec<String> = {
            // recover a poisoned lock: the map only holds cooldown timestamps, so its contents stay usable
            let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
            // a key past its cooldown holds nothing back, so dropping it keeps the map as small as the live alerts
            sent.retain(|_, t| now.duration_since(*t) < self.cooldown);
            alerts
                .iter()
                .filter(|a| {
                    let due = !sent.contains_key(&a.key);
                    if due {
                        sent.insert(a.key.clone(), now);
                    }
                    due
                })
                .map(|a| a.text.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keys_past_their_cooldown_are_pruned() {
        let s = AlertSender::new(None, std::time::Duration::from_secs(60));
        let t0 = Instant::now();
        s.send(&[Alert::new("a", "x"), Alert::new("b", "y")], t0).await;
        s.send(&[Alert::new("c", "z")], t0 + std::time::Duration::from_secs(61)).await;
        assert_eq!(s.sent.lock().unwrap().len(), 1, "only the live key is kept");
    }

    #[tokio::test]
    async fn a_poisoned_lock_does_not_panic_later_alerts() {
        let s = std::sync::Arc::new(AlertSender::new(None, std::time::Duration::from_secs(60)));
        let s2 = s.clone();
        let _ = std::thread::spawn(move || {
            let _g = s2.sent.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(s.sent.is_poisoned());
        let sent = s.send(&[Alert::new("a", "x")], Instant::now()).await;
        assert_eq!(sent, vec!["x".to_string()]);
    }
}
