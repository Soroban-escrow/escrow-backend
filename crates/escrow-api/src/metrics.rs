//! Prometheus text metrics: in-process counters for what this API handled, plus gauges read from the database.

use escrow_db::Db;
use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

#[derive(Default)]
pub struct Metrics {
    pub http_requests: AtomicU64,
    pub http_server_errors: AtomicU64,
    pub http_rate_limited: AtomicU64,
    pub webhooks_accepted: AtomicU64,
    pub webhooks_duplicate: AtomicU64,
    pub webhooks_rejected: AtomicU64,
    pub logins: AtomicU64,
    pub wallets_linked: AtomicU64,
    pub tx_submit_rejected: AtomicU64,
    pub webhooks_ignored: AtomicU64,
}

impl Metrics {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Relaxed);
    }

    pub async fn render(&self, db: &Db) -> String {
        let mut out = String::new();
        let mut counter = |name: &str, help: &str, v: u64| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
        };
        counter("skillescrow_http_requests_total", "HTTP requests handled", self.http_requests.load(Relaxed));
        counter("skillescrow_http_server_errors_total", "HTTP 5xx responses", self.http_server_errors.load(Relaxed));
        counter("skillescrow_http_rate_limited_total", "Requests rejected by the rate limiter", self.http_rate_limited.load(Relaxed));
        counter("skillescrow_webhooks_accepted_total", "GitHub webhooks accepted and queued", self.webhooks_accepted.load(Relaxed));
        counter("skillescrow_webhooks_duplicate_total", "GitHub webhook redeliveries ignored", self.webhooks_duplicate.load(Relaxed));
        counter("skillescrow_webhooks_rejected_total", "Webhooks with a bad signature or body", self.webhooks_rejected.load(Relaxed));
        counter("skillescrow_logins_total", "Completed GitHub sign-ins", self.logins.load(Relaxed));
        counter("skillescrow_wallets_linked_total", "Wallets linked", self.wallets_linked.load(Relaxed));
        counter("skillescrow_tx_submit_rejected_total", "Signed transactions rejected by the relay", self.tx_submit_rejected.load(Relaxed));
        counter("skillescrow_webhooks_ignored_total", "Verified webhooks for events the backend does not handle", self.webhooks_ignored.load(Relaxed));

        let mut gauge = |name: &str, help: &str, rows: Vec<(String, i64)>| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
            for (label, v) in rows {
                let _ = if label.is_empty() { writeln!(out, "{name} {v}") } else { writeln!(out, "{name}{{{label}}} {v}") };
            }
        };
        // a failed gauge query is logged and flips skillescrow_metrics_db_up to 0 instead of
        // silently rendering empty series
        let mut db_up = true;
        let by = |name: &'static str, sql: &'static str, key: &'static str| {
            let db = &db.pool;
            async move {
                match sqlx::query_as::<_, (String, i64)>(sql).fetch_all(db).await {
                    Ok(rows) => rows.into_iter().map(|(s, n)| (format!("{key}=\"{s}\""), n)).collect::<Vec<_>>(),
                    Err(e) => {
                        tracing::warn!(gauge = name, error = %e, "metrics gauge query failed");
                        db_up = false;
                        vec![]
                    }
                }
            }
        };
        gauge("skillescrow_bounties", "Bounties by status", by("bounties", "SELECT status, count(*) FROM bounties GROUP BY status", "status").await);
        gauge("skillescrow_payouts", "Payouts by status", by("payouts", "SELECT status, count(*) FROM payouts GROUP BY status", "status").await);
        gauge("skillescrow_jobs", "Background jobs by status", by("jobs", "SELECT status, count(*) FROM jobs GROUP BY status", "status").await);
        let lag = match sqlx::query_scalar::<_, i32>("SELECT ledger FROM indexer_cursors WHERE id = 'bounty_escrow'")
            .fetch_optional(&db.pool)
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(gauge = "indexer_ledger", error = %e, "metrics gauge query failed");
                db_up = false;
                None
            }
        };
        gauge(
            "skillescrow_indexer_ledger",
            "Last ledger the indexer has reached",
            lag.map(|l| vec![(String::new(), i64::from(l))]).unwrap_or_default(),
        );
        gauge("skillescrow_metrics_db_up", "Whether the database gauge queries succeeded (1) or failed (0)", vec![(String::new(), i64::from(db_up))]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "escrow_db::MIGRATOR")]
    async fn a_closed_pool_reports_db_down(pool: sqlx::PgPool) {
        pool.close().await;
        let db = Db::from_pool(pool);
        let m = Metrics::default().render(&db).await;
        assert!(m.contains("skillescrow_metrics_db_up 0"), "{m}");
    }
}
