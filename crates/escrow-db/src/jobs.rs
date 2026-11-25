//! A durable job queue in Postgres. Replaces Redis/BullMQ: one less service to run, and a job is
//! enqueued in the same transaction as the row that caused it, so neither can be lost without the other.
//!
//! Workers claim with `FOR UPDATE SKIP LOCKED` and hold a lease; a worker that dies simply lets its
//! lease expire and the job is picked up again.

use crate::models::Job;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

const COLS: &str = "id, kind, payload, status, run_at, attempts, max_attempts, dedupe_key, last_error";

/// Adds a job. With a `dedupe_key`, a still-queued job with that key makes this a no-op (returns `false`).
pub async fn enqueue<'e>(
    ex: impl PgExecutor<'e>,
    kind: &str,
    payload: &serde_json::Value,
    run_at: DateTime<Utc>,
    max_attempts: i32,
    dedupe_key: Option<&str>,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (kind, payload, run_at, max_attempts, dedupe_key) VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (dedupe_key) WHERE status = 'queued' AND dedupe_key IS NOT NULL DO NOTHING",
    )
    .bind(kind)
    .bind(payload)
    .bind(run_at)
    .bind(max_attempts)
    .bind(dedupe_key)
    .execute(ex)
    .await
    .map(|r| r.rows_affected() == 1)
}

/// Debounce: a newer request replaces the waiting job's run time and payload. A job that is already
/// running is left alone and the new one queues behind it.
pub async fn enqueue_debounced<'e>(
    ex: impl PgExecutor<'e>,
    kind: &str,
    payload: &serde_json::Value,
    run_at: DateTime<Utc>,
    max_attempts: i32,
    dedupe_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (kind, payload, run_at, max_attempts, dedupe_key) VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (dedupe_key) WHERE status = 'queued' AND dedupe_key IS NOT NULL
         DO UPDATE SET run_at = EXCLUDED.run_at, payload = EXCLUDED.payload, updated_at = now()",
    )
    .bind(kind)
    .bind(payload)
    .bind(run_at)
    .bind(max_attempts)
    .bind(dedupe_key)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Takes the next due job and leases it for `lease_secs`. Returns `None` when nothing is due.
/// A job whose lease expired (its worker died) is eligible again while attempts remain.
pub async fn claim<'e>(ex: impl PgExecutor<'e>, kinds: &[&str], lease_secs: i64) -> Result<Option<Job>, sqlx::Error> {
    let kinds: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE jobs SET status = 'running', attempts = attempts + 1, locked_until = now() + make_interval(secs => $2), updated_at = now()
         WHERE id = (
           SELECT id FROM jobs
           WHERE kind = ANY($1)
             AND ((status = 'queued' AND run_at <= now()) OR (status = 'running' AND locked_until < now() AND attempts < max_attempts))
           ORDER BY run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING {COLS}"
    )))
    .bind(kinds)
    .bind(lease_secs as f64)
    .fetch_optional(ex)
    .await
}

pub async fn complete<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE jobs SET status = 'done', locked_until = NULL, last_error = NULL, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(ex)
        .await
        .map(|_| ())
}

/// Records a failure. Retries after `backoff_secs` while attempts remain, otherwise the job is `dead`
/// (kept for inspection, never retried). Returns `true` when the job was given up on.
pub async fn fail<'e>(ex: impl PgExecutor<'e>, id: i64, error: &str, backoff_secs: i64) -> Result<bool, sqlx::Error> {
    let dead: bool = sqlx::query_scalar(
        "UPDATE jobs SET
           status = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END,
           run_at = CASE WHEN attempts >= max_attempts THEN run_at ELSE now() + make_interval(secs => $3) END,
           last_error = $2, locked_until = NULL, updated_at = now()
         WHERE id = $1 RETURNING status = 'dead'",
    )
    .bind(id)
    .bind(error)
    .bind(backoff_secs as f64)
    .fetch_one(ex)
    .await?;
    Ok(dead)
}

/// Jobs whose lease expired with no attempts left can never run again: mark them dead so they show up.
pub async fn reap_exhausted<'e>(ex: impl PgExecutor<'e>) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE jobs SET status = 'dead', last_error = COALESCE(last_error, 'lease expired'), updated_at = now() WHERE status = 'running' AND locked_until < now() AND attempts >= max_attempts")
        .execute(ex)
        .await
        .map(|r| r.rows_affected())
}

pub async fn count_dead<'e>(ex: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status = 'dead'").fetch_one(ex).await
}

pub async fn count_pending<'e>(ex: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status IN ('queued','running')").fetch_one(ex).await
}

/// Housekeeping: finished jobs older than `before` are deleted.
pub async fn purge_done<'e>(ex: impl PgExecutor<'e>, before: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM jobs WHERE status = 'done' AND updated_at < $1").bind(before).execute(ex).await.map(|r| r.rows_affected())
}

/// A job that can never succeed (bad payload, permission denied): park it as dead right away, keeping the reason.
pub async fn bury<'e>(ex: impl PgExecutor<'e>, id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE jobs SET status = 'dead', last_error = $2, locked_until = NULL, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(ex)
        .await
        .map(|_| ())
}
