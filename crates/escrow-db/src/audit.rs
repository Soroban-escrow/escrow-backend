// Append-only audit trail. It also serves as the "already sent" memory for
// reminders and announcements.
//
// Rows are hash-chained by the `audit_log_chain` trigger (migration 0008):
// each row stores `seq`, `prev_hash` and `hash`. Use [`verify`] to walk
// the chain.
//
// Transaction relay submissions are recorded through [`log_tx_submit`] so
// successful relays can be attributed to the authenticated API user without
// storing signed transaction payloads or secrets.

use crate::new_id;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// Records an audit event.
///
/// Audit rows are append-only and hash-chained by the database trigger.
pub async fn log<'e>(
    ex: impl PgExecutor<'e>,
    actor: &str,
    action: &str,
    target: &str,
    data: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (id, actor, action, target, data)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(new_id())
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(data)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Records a successfully relayed signed transaction.
///
/// `actor` should identify the authenticated API user using the application's
/// existing actor convention.
///
/// `tx_hash` is stored as the audit target. The invoked contract function is
/// stored in the audit metadata.
///
/// This deliberately does not store the signed transaction envelope, secret
/// keys, or other sensitive transaction payload data.
pub async fn log_tx_submit<'e>(
    ex: impl PgExecutor<'e>,
    actor: &str,
    tx_hash: &str,
    function: &str,
) -> Result<(), sqlx::Error> {
    let data = serde_json::json!({
        "function": function,
    });

    log(ex, actor, "tx.submit", tx_hash, &data).await
}

/// Checks whether an audit event with the given action and target already exists.
pub async fn exists<'e>(
    ex: impl PgExecutor<'e>,
    action: &str,
    target: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
            FROM audit_log
            WHERE action = $1 AND target = $2
        )",
    )
    .bind(action)
    .bind(target)
    .fetch_one(ex)
    .await
}

/// Returns recent audit entries, newest first.
pub async fn recent<'e>(
    ex: impl PgExecutor<'e>,
    limit: i64,
) -> Result<Vec<crate::models::AuditEntry>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, actor, action, target, data, created_at
         FROM audit_log
         ORDER BY created_at DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(ex)
    .await
}

/// #133 — Keyset-paginated filter over the audit log.
///
/// Rows are ordered newest-first by `(created_at, id)` so the caller can walk
/// the log without snapshotting or repeating rows on concurrent writes.
///
/// `actor` and `action` are exact-match filters; `target_prefix` matches any
/// `target` starting with the given string (so a caller can scope the browse
/// to a bounty, dispute, repo or payout). `cursor` is the `id` of the row that
/// ended the previous page.
///
/// The `audit_log_lookup (action, target)` index covers the common
/// filter-by-action browse. Filters whose action is not fixed fall back to
/// a sorted scan that is bounded by `limit`.
pub struct Filter<'a> {
    pub actor: Option<&'a str>,
    pub action: Option<&'a str>,
    pub target_prefix: Option<&'a str>,
    pub cursor: Option<&'a str>,
    pub limit: i64,
}

pub async fn list(
    pool: &sqlx::PgPool,
    f: &Filter<'_>,
) -> Result<Vec<crate::models::AuditEntry>, sqlx::Error> {
    // #133 — The WHERE clauses below mirror the `audit_log_lookup` index on
    // (action, target); other dimensions are post-filtered inside the index
    // scan so the plan stays sort-free.
    let target_pattern =
        f.target_prefix.map(|p| format!("{}%", p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
    let (anchor_created_at, anchor_id) = match f.cursor {
        None => (None, None),
        Some(id) => {
            let row: Option<(DateTime<Utc>, String)> =
                sqlx::query_as("SELECT created_at, id FROM audit_log WHERE id = $1").bind(id).fetch_optional(pool).await?;
            match row {
                Some((ts, id)) => (Some(ts), Some(id)),
                // Cursor row is gone (deleted or never existed). Return an
                // empty page instead of restarting from the top so the client
                // cannot accidentally loop over the same rows.
                None => return Ok(Vec::new()),
            }
        }
    };
    sqlx::query_as(
        "SELECT id, actor, action, target, data, created_at
           FROM audit_log
          WHERE ($1::text IS NULL OR actor  = $1)
            AND ($2::text IS NULL OR action = $2)
            AND ($3::text IS NULL OR target LIKE $3 ESCAPE '\\')
            AND (
                  $4::timestamptz IS NULL
               OR (created_at, id) < ($4, $5::text)
            )
          ORDER BY created_at DESC, id DESC
          LIMIT $6",
    )
    .bind(f.actor)
    .bind(f.action)
    .bind(target_pattern)
    .bind(anchor_created_at)
    .bind(anchor_id)
    .bind(f.limit)
    .fetch_all(pool)
    .await
}

/// Counts `anomaly_detected` audit entries in the last 24 hours.
pub async fn count_anomalies_24h<'e>(
    ex: impl PgExecutor<'e>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM audit_log
         WHERE action = 'anomaly_detected'
           AND created_at > NOW() - INTERVAL '24 hours'",
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

/// Walks the chain from `from_seq` (1 for the whole table) and returns the
/// first broken link, if any.
///
/// Verification runs inside Postgres via `audit_verify()`, so it needs no
/// hashing crate.
pub async fn verify<'e>(
    ex: impl PgExecutor<'e>,
    from_seq: i64,
) -> Result<Option<BrokenLink>, sqlx::Error> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT broken_seq, broken_id, reason
         FROM audit_verify($1)",
    )
    .bind(from_seq)
    .fetch_optional(ex)
    .await?;

    Ok(row.map(|(seq, id, reason)| BrokenLink { seq, id, reason }))
}
