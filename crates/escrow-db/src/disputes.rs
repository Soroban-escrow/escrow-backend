use crate::{models::Dispute, new_id};
use sqlx::{PgExecutor, PgPool};

const COLS: &str = "id, bounty_id, opened_by_github_id, reason, evidence, status, resolution_note, pending_note, pending_winner, pending_winner_bps, created_at, updated_at";

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

/// What [`create_if_none_open`] did.
#[derive(Debug, Clone)]
pub enum NewDispute {
    /// A new dispute row was inserted.
    Created(Dispute),
    /// The bounty already had an open (`requested` or `opened_onchain`) dispute; nothing was written.
    AlreadyOpen(Dispute),
}

/// Records a dispute unless the bounty already has an open one. The guard is the partial unique index
/// `disputes_one_open_per_bounty`, so concurrent requests cannot both insert.
pub async fn create_if_none_open(pool: &PgPool, bounty_id: i64, opened_by: i64, reason: &str) -> Result<NewDispute, sqlx::Error> {
    // The open dispute can be resolved between our conflicting insert and the lookup; retry the pair then.
    for _ in 0..3 {
        let inserted: Option<Dispute> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "INSERT INTO disputes (id, bounty_id, opened_by_github_id, reason, status) VALUES ($1,$2,$3,$4,'requested') \
             ON CONFLICT (bounty_id) WHERE status IN ('requested','opened_onchain') DO NOTHING RETURNING {COLS}"
        )))
        .bind(new_id())
        .bind(bounty_id)
        .bind(opened_by)
        .bind(reason)
        .fetch_optional(pool)
        .await?;
        if let Some(d) = inserted {
            return Ok(NewDispute::Created(d));
        }
        if let Some(d) = open_for_bounty(pool, bounty_id).await? {
            return Ok(NewDispute::AlreadyOpen(d));
        }
    }
    Err(sqlx::Error::RowNotFound)
}

/// The bounty's open dispute, if any.
pub async fn open_for_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<Option<Dispute>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM disputes WHERE bounty_id = $1 AND status IN ('requested','opened_onchain')"
    )))
    .bind(bounty_id)
    .fetch_optional(ex)
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

pub async fn set_pending_resolution<'e>(
    ex: impl PgExecutor<'e>,
    id: &str,
    note: &str,
    winner: Option<&str>,
    winner_bps: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE disputes SET pending_note = $2, pending_winner = $3, pending_winner_bps = $4, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(note)
    .bind(winner)
    .bind(winner_bps as i32)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn promote_pending_resolution<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "UPDATE disputes SET resolution_note = pending_note, pending_note = NULL, pending_winner = NULL, pending_winner_bps = NULL, updated_at = now() WHERE id = $1 AND pending_note IS NOT NULL",
    )
    .bind(id)
    .execute(ex)
    .await
    .map(|r| r.rows_affected() > 0)
}

pub async fn promote_pending_resolutions_for_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE disputes SET resolution_note = pending_note, pending_note = NULL, pending_winner = NULL, pending_winner_bps = NULL, updated_at = now() WHERE bounty_id = $1 AND pending_note IS NOT NULL",
    )
    .bind(bounty_id)
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
