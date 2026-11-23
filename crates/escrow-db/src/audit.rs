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

/// Filters for [`browse`]: exact actor/action, target prefix, and a keyset cursor (last seq seen).
#[derive(Default)]
pub struct BrowseFilter<'a> {
    pub actor: Option<&'a str>,
    pub action: Option<&'a str>,
    pub target_prefix: Option<&'a str>,
    pub before_seq: Option<i64>,
    pub limit: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct AuditRow {
    #[sqlx(flatten)]
    pub entry: crate::models::AuditEntry,
    pub seq: i64,
}

/// Audit entries newest-first with optional filters; keyset-paginated on the hash-chain `seq`.
/// Returns the rows plus the seq to pass as `before_seq` for the next page.
pub async fn browse<'e>(ex: impl PgExecutor<'e>, f: &BrowseFilter<'_>) -> Result<(Vec<crate::models::AuditEntry>, Option<i64>), sqlx::Error> {
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, actor, action, target, data, created_at, seq FROM audit_log
         WHERE ($1::text IS NULL OR actor = $1)
           AND ($2::text IS NULL OR action = $2)
           AND ($3::text IS NULL OR target LIKE $3 || '%')
           AND ($4::bigint IS NULL OR seq < $4)
         ORDER BY seq DESC LIMIT $5",
    )
    .bind(f.actor)
    .bind(f.action)
    .bind(f.target_prefix)
    .bind(f.before_seq)
    .bind(f.limit + 1)
    .fetch_all(ex)
    .await?;
    let more = rows.len() as i64 > f.limit;
    let rows: Vec<AuditRow> = rows.into_iter().take(f.limit as usize).collect();
    let next = if more { rows.last().map(|r| r.seq) } else { None };
    Ok((rows.into_iter().map(|r| r.entry).collect(), next))
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
