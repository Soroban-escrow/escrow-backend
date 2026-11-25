//! Append-only audit trail. It also serves as the "already sent" memory for reminders and announcements.
//!
//! Rows are hash-chained by the `audit_log_chain` trigger (migration 0008): each row stores
//! `seq`, `prev_hash` and `hash`. Use [`verify`] to walk the chain.

use chrono::{DateTime, Utc};
use crate::new_id;
use sqlx::PgExecutor;

pub async fn log<'e>(
    ex: impl PgExecutor<'e>,
    actor: &str,
    action: &str,
    target: &str,
    data: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_log (id, actor, action, target, data) VALUES ($1,$2,$3,$4,$5)")
        .bind(new_id())
        .bind(actor)
        .bind(action)
        .bind(target)
        .bind(data)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn exists<'e>(ex: impl PgExecutor<'e>, action: &str, target: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM audit_log WHERE action = $1 AND target = $2)")
        .bind(action)
        .bind(target)
        .fetch_one(ex)
        .await
}

pub async fn recent<'e>(ex: impl PgExecutor<'e>, limit: i64) -> Result<Vec<crate::models::AuditEntry>, sqlx::Error> {
    sqlx::query_as("SELECT id, actor, action, target, data, created_at FROM audit_log ORDER BY created_at DESC LIMIT $1")
        .bind(limit)
        .fetch_all(ex)
        .await
}

/// Counts anomaly_detected audit entries in the last 24 hours.
pub async fn count_anomalies_24h<'e>(ex: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'anomaly_detected' AND created_at > NOW() - INTERVAL '24 hours'"
    )
    .fetch_one(ex)
    .await
}

/// The first row where the chain is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenLink {
    pub seq: i64,
    pub id: String,
    pub reason: String,
}

/// Walks the chain from `from_seq` (1 for the whole table) and returns the first broken link, if any.
/// Verification runs inside Postgres via `audit_verify()`, so it needs no hashing crate.
pub async fn verify<'e>(ex: impl PgExecutor<'e>, from_seq: i64) -> Result<Option<BrokenLink>, sqlx::Error> {
    let row: Option<(i64, String, String)> = sqlx::query_as("SELECT broken_seq, broken_id, reason FROM audit_verify($1)")
        .bind(from_seq)
        .fetch_optional(ex)
        .await?;
    Ok(row.map(|(seq, id, reason)| BrokenLink { seq, id, reason }))
}
