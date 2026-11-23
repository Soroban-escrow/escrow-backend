//! The worker: a Postgres-backed job queue plus the long-running loops.
//!
//! Jobs are claimed with `FOR UPDATE SKIP LOCKED` and leased, so several workers can run side by side
//! and a crashed one simply lets its lease expire. Failures retry with exponential backoff; failures that
//! can never succeed (a malformed payload, a permission denied) are parked as dead immediately, with the reason.

use crate::{
    dispatch::{DispatchDeps, dispatch_webhook},
    error::EngineError,
    jobs as kinds,
    ops::{AlertInputs, AlertSender, collect_alerts},
    ports::ReviewRunner,
    sync::{GithubCommenter, SyncDeps, announce_payout, refresh_bounty_card, send_refund_reminders},
};
use chrono::{Duration as ChronoDuration, Utc};
use escrow_chain::{ChainReader, ReleaseSigner};
use escrow_db::{Db, Job, jobs, sessions, webhooks};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const LEASE_SECS: i64 = 180;

pub struct WorkerDeps {
    pub db: Db,
    pub dispatch: DispatchDeps,
    pub sync: SyncDeps,
    pub reviews: Option<Arc<dyn ReviewRunner>>,
    pub chain: Arc<dyn ChainReader>,
    pub relayer: Option<Arc<dyn ReleaseSigner>>,
    pub alerts: Arc<AlertSender>,
    /// False when the GitHub App is not configured: GitHub jobs then stay queued instead of retrying until they die.
    pub github_configured: bool,
    pub min_relayer_xlm: f64,
    pub grace: ChronoDuration,
}

/// Seconds to wait before retrying a job that has failed `attempts` times: 4, 8, 16, ... capped at 15 minutes.
pub fn retry_backoff_secs(attempts: i32) -> i64 {
    (1i64 << (attempts.clamp(1, 20) + 1)).min(15 * 60)
}

fn payload_i64(job: &Job, key: &str) -> Result<i64, EngineError> {
    job.payload.get(key).and_then(|v| v.as_i64()).ok_or_else(|| EngineError::Payload(format!("job {} has no `{key}`", job.id)))
}

/// Runs one job. The returned label is for logs.
pub async fn handle_job(w: &WorkerDeps, job: &Job) -> Result<String, EngineError> {
    match job.kind.as_str() {
        kinds::WEBHOOK => {
            let id = job.payload.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let event = job.payload.get("event").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let payload = job.payload.get("payload").cloned().unwrap_or_default();
            match dispatch_webhook(&w.dispatch, &event, &payload).await {
                Ok(label) => {
                    webhooks::mark_processed(&w.db.pool, &id, None).await?;
                    Ok(format!("{event}: {label}"))
                }
                Err(e) => {
                    // record the failure on the delivery as soon as retrying is pointless or exhausted
                    if !e.is_transient() || job.attempts >= job.max_attempts {
                        webhooks::mark_processed(&w.db.pool, &id, Some(&e.to_string())).await?;
                    }
                    Err(e)
                }
            }
        }
        kinds::CARD_REFRESH => Ok(format!("card {:?}", refresh_bounty_card(&w.sync, payload_i64(job, "bountyId")?).await?)),
        kinds::PAYOUT_ANNOUNCE => Ok(format!("announced={}", announce_payout(&w.sync, payload_i64(job, "bountyId")?).await?)),
        kinds::REVIEW => {
            let Some(runner) = &w.reviews else {
                return Err(EngineError::Other("AI review is not configured".into()));
            };
            let force = job.payload.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            match runner.run(payload_i64(job, "prId")?, force).await {
                Ok(label) => Ok(format!("review {label}")),
                Err(f) if f.transient => Err(EngineError::Chain(escrow_chain::ChainError::Http(f.message))),
                Err(f) => Err(EngineError::Other(f.message)),
            }
        }
        other => Err(EngineError::Other(format!("unknown job kind `{other}`"))),
    }
}

/// Claims and runs jobs until stopped. `concurrency` workers share the queue.
pub async fn run_queue(w: Arc<WorkerDeps>, stop: CancellationToken, concurrency: usize, poll: Duration) {
    let mut kinds_to_run: Vec<&str> = vec![kinds::WEBHOOK, kinds::CARD_REFRESH, kinds::PAYOUT_ANNOUNCE];
    if w.reviews.is_some() {
        kinds_to_run.push(kinds::REVIEW);
    }
    if !w.github_configured {
        kinds_to_run.retain(|k| !kinds::GITHUB_KINDS.contains(k));
        tracing::warn!("GitHub App is not configured: jobs that need it stay queued until it is");
    }
    if kinds_to_run.is_empty() {
        stop.cancelled().await;
        return;
    }
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..concurrency.max(1) {
        let (w, stop, kinds_to_run) = (w.clone(), stop.clone(), kinds_to_run.clone());
        tasks.spawn(async move {
            while !stop.is_cancelled() {
                match jobs::claim(&w.db.pool, &kinds_to_run, LEASE_SECS).await {
                    Ok(Some(job)) => run_one(&w, &job, n).await,
                    Ok(None) => {
                        tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(poll) => {} }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "could not claim a job");
                        tokio::time::sleep(poll).await;
                    }
                }
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

async fn run_one(w: &WorkerDeps, job: &Job, worker: usize) {
    let pool = &w.db.pool;
    match handle_job(w, job).await {
        Ok(label) => {
            tracing::info!(worker, job = job.id, kind = %job.kind, %label, "job done");
            if let Err(e) = jobs::complete(pool, job.id).await {
                tracing::error!(job = job.id, error = %e, "could not mark the job done");
            }
        }
        Err(e) if e.is_transient() => {
            let mut wait = retry_backoff_secs(job.attempts);
            // a rate limit names the time GitHub will accept calls again: never retry before it
            if let Some(at) = e.retry_at() {
                wait = wait.max((at - Utc::now()).num_seconds().max(1));
            }
            tracing::warn!(worker, job = job.id, kind = %job.kind, attempt = job.attempts, error = %e, retry_in_secs = wait, "job failed, will retry");
            match jobs::fail(pool, job.id, &e.to_string(), wait).await {
                Ok(true) => tracing::error!(job = job.id, kind = %job.kind, "job gave up after its last attempt"),
                Ok(false) => {}
                Err(db) => tracing::error!(job = job.id, error = %db, "could not record the failure"),
            }
        }
        Err(e) => {
            tracing::error!(worker, job = job.id, kind = %job.kind, error = %e, "job can never succeed; parked as dead");
            let _ = jobs::bury(pool, job.id, &e.to_string()).await;
        }
    }
}

/// Hourly reminders, per-minute alerts, daily housekeeping.
pub async fn run_periodic(w: Arc<WorkerDeps>, stop: CancellationToken) {
    let mut reminders = tokio::time::interval(Duration::from_secs(3600));
    let mut alerts = tokio::time::interval(Duration::from_secs(60));
    let mut housekeeping = tokio::time::interval(Duration::from_secs(6 * 3600));
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = reminders.tick() => {
                let commenter = GithubCommenter { db: w.db.clone(), gh: w.sync.gh.clone() };
                match send_refund_reminders(&w.db, &commenter, w.grace, &w.sync.web_url, Utc::now()).await {
                    Ok(o) => {
                        if !o.sent.is_empty() {
                            tracing::info!(sent = ?o.sent, "refund reminders");
                        }
                        if !o.failed.is_empty() {
                            tracing::warn!(failed = ?o.failed, "refund reminders failed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "reminders skipped"),
                }
            }
            _ = alerts.tick() => {
                let balance = match &w.relayer { Some(r) => r.balance_xlm().await.ok(), None => None };
                let tip = w.chain.latest_ledger().await.ok();
                match collect_alerts(&w.db, &AlertInputs { relayer_balance_xlm: balance, min_relayer_xlm: w.min_relayer_xlm, latest_ledger: tip, now: Utc::now() }).await {
                    Ok(msgs) => { w.alerts.send(&msgs, std::time::Instant::now()).await; }
                    Err(e) => tracing::warn!(error = %e, "alert check failed"),
                }
            }
            _ = housekeeping.tick() => {
                let now = Utc::now();
                let _ = sessions::purge_expired(&w.db.pool, now).await;
                let _ = sessions::purge_challenges(&w.db.pool, now - ChronoDuration::days(1)).await;
                let _ = jobs::purge_done(&w.db.pool, now - ChronoDuration::days(7)).await;
                let _ = jobs::reap_exhausted(&w.db.pool).await;
                // rate-limit windows older than a day are never read again (the shared limiter counts per window)
                let _ = escrow_db::rate_limits::prune(&w.db.pool, 86_400).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps_at_fifteen_minutes() {
        assert_eq!(retry_backoff_secs(1), 4);
        assert_eq!(retry_backoff_secs(2), 8);
        assert_eq!(retry_backoff_secs(5), 64);
        assert_eq!(retry_backoff_secs(20), 900);
        assert_eq!(retry_backoff_secs(0), 4);
        assert_eq!(retry_backoff_secs(-3), 4);
    }
}
