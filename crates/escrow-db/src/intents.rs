//! Funding links: what `/bounty 50 USDC 14d` creates and the web app redeems.

use crate::models::{FundingIntent, INTENT_COLS};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

pub struct NewIntent<'a> {
    pub id: &'a str,
    pub repo_id: i64,
    pub issue_number: i32,
    pub requested_by: i64,
    pub token: &'a str,
    pub amount: i128,
    pub duration_days: i32,
    pub comment_id: i64,
    pub expires_at: DateTime<Utc>,
}

pub async fn create<'e>(ex: impl PgExecutor<'e>, i: &NewIntent<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO funding_intents (id, repo_id, issue_number, requested_by, token, amount, duration_days, comment_id, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6::numeric,$7,$8,$9)",
    )
    .bind(i.id)
    .bind(i.repo_id)
    .bind(i.issue_number)
    .bind(i.requested_by)
    .bind(i.token)
    .bind(i.amount.to_string())
    .bind(i.duration_days)
    .bind(i.comment_id)
    .bind(i.expires_at)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<FundingIntent>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {INTENT_COLS} FROM funding_intents WHERE id = $1")))
        .bind(id)
        .fetch_optional(ex)
        .await
}

/// The newest unused link for exactly this on-chain funding, so the indexer can attribute it to the GitHub user who asked.
pub async fn find_unused_match<'e>(
    ex: impl PgExecutor<'e>,
    repo_id: i64,
    issue_number: i32,
    token: &str,
    amount: i128,
) -> Result<Option<FundingIntent>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {INTENT_COLS} FROM funding_intents
         WHERE repo_id = $1 AND issue_number = $2 AND token = $3 AND amount = $4::numeric AND used_at IS NULL
         ORDER BY created_at DESC LIMIT 1"
    )))
    .bind(repo_id)
    .bind(issue_number)
    .bind(token)
    .bind(amount.to_string())
    .fetch_optional(ex)
    .await
}

pub async fn mark_used<'e>(ex: impl PgExecutor<'e>, id: &str, at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE funding_intents SET used_at = $2 WHERE id = $1").bind(id).bind(at).execute(ex).await.map(|_| ())
}

pub async fn set_comment<'e>(ex: impl PgExecutor<'e>, id: &str, comment_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE funding_intents SET comment_id = $2 WHERE id = $1").bind(id).bind(comment_id).execute(ex).await.map(|_| ())
}
