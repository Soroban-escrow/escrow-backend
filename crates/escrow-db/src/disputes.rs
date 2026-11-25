use crate::{models::Dispute, new_id};
use sqlx::PgExecutor;

const COLS: &str = "id, bounty_id, opened_by_github_id, reason, evidence, status, resolution_note, created_at, updated_at";

pub async fn create<'e>(ex: impl PgExecutor<'e>, bounty_id: i64, opened_by: i64, reason: &str) -> Result<Dispute, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO disputes (id, bounty_id, opened_by_github_id, reason, status) VALUES ($1,$2,$3,$4,'requested') RETURNING {COLS}"
    )))
    .bind(new_id())
    .bind(bounty_id)
    .bind(opened_by)
    .bind(reason)
    .fetch_one(ex)
    .await
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<Dispute>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM disputes WHERE id = $1"))).bind(id).fetch_optional(ex).await
}

/// The queue the admin page works through, oldest first.
pub async fn list_open<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<Dispute>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM disputes WHERE status IN ('requested','opened_onchain') ORDER BY created_at"
    )))
    .fetch_all(ex)
    .await
}

pub async fn count_open<'e>(ex: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM disputes WHERE status IN ('requested','opened_onchain')").fetch_one(ex).await
}

pub async fn count_open_older_than<'e>(ex: impl PgExecutor<'e>, before: chrono::DateTime<chrono::Utc>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM disputes WHERE status IN ('requested','opened_onchain') AND created_at < $1")
        .bind(before)
        .fetch_one(ex)
        .await
}

pub async fn set_status<'e>(ex: impl PgExecutor<'e>, id: &str, status: &str, note: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE disputes SET status = $2, resolution_note = COALESCE($3, resolution_note), updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(note)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn set_note<'e>(ex: impl PgExecutor<'e>, id: &str, note: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE disputes SET resolution_note = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(note)
        .execute(ex)
        .await
        .map(|_| ())
}

/// The contract emitted `disputed`: requested disputes on that bounty are now open on chain.
pub async fn mark_opened_onchain<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE disputes SET status = 'opened_onchain', updated_at = now() WHERE bounty_id = $1 AND status = 'requested'")
        .bind(bounty_id)
        .execute(ex)
        .await
        .map(|r| r.rows_affected())
}

/// The contract emitted `resolved`.
pub async fn mark_resolved<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE disputes SET status = 'resolved', updated_at = now() WHERE bounty_id = $1 AND status IN ('requested','opened_onchain')",
    )
    .bind(bounty_id)
    .execute(ex)
    .await
    .map(|r| r.rows_affected())
}
