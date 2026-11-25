//! AI reviews. Advisory records only: nothing in the payout path reads them.

use crate::{models::AiReview, new_id};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

const COLS: &str = "id, pr_id, head_sha, model, verdict, score, result, flags, input_tokens, output_tokens, comment_id, created_at";

pub async fn get<'e>(ex: impl PgExecutor<'e>, pr_id: i64, head_sha: &str) -> Result<Option<AiReview>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM ai_reviews WHERE pr_id = $1 AND head_sha = $2")))
        .bind(pr_id)
        .bind(head_sha)
        .fetch_optional(ex)
        .await
}

pub async fn latest_for_pr<'e>(ex: impl PgExecutor<'e>, pr_id: i64) -> Result<Vec<AiReview>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM ai_reviews WHERE pr_id = $1 ORDER BY created_at DESC LIMIT 1")))
        .bind(pr_id)
        .fetch_all(ex)
        .await
}

/// The newest review that already has a GitHub comment: the one to edit in place.
pub async fn latest_commented<'e>(ex: impl PgExecutor<'e>, pr_id: i64) -> Result<Option<AiReview>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM ai_reviews WHERE pr_id = $1 AND comment_id IS NOT NULL ORDER BY created_at DESC LIMIT 1"
    )))
    .bind(pr_id)
    .fetch_optional(ex)
    .await
}

pub struct NewReview<'a> {
    pub pr_id: i64,
    pub head_sha: &'a str,
    pub model: &'a str,
    pub verdict: &'a str,
    pub score: i32,
    pub result: &'a serde_json::Value,
    pub flags: &'a [String],
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub comment_id: Option<i64>,
}

pub async fn upsert<'e>(ex: impl PgExecutor<'e>, r: &NewReview<'_>) -> Result<AiReview, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ai_reviews (id, pr_id, head_sha, model, verdict, score, result, flags, input_tokens, output_tokens, comment_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
         ON CONFLICT (pr_id, head_sha) DO UPDATE SET model = EXCLUDED.model, verdict = EXCLUDED.verdict, score = EXCLUDED.score,
           result = EXCLUDED.result, flags = EXCLUDED.flags, input_tokens = EXCLUDED.input_tokens, output_tokens = EXCLUDED.output_tokens
         RETURNING {COLS}"
    )))
    .bind(new_id())
    .bind(r.pr_id)
    .bind(r.head_sha)
    .bind(r.model)
    .bind(r.verdict)
    .bind(r.score)
    .bind(r.result)
    .bind(r.flags)
    .bind(r.input_tokens)
    .bind(r.output_tokens)
    .bind(r.comment_id)
    .fetch_one(ex)
    .await
}

pub async fn set_comment<'e>(ex: impl PgExecutor<'e>, id: &str, comment_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE ai_reviews SET comment_id = $2 WHERE id = $1").bind(id).bind(comment_id).execute(ex).await.map(|_| ())
}

/// Reviews of this PR since `since` (per-PR daily cap).
pub async fn count_for_pr_since<'e>(ex: impl PgExecutor<'e>, pr_id: i64, since: DateTime<Utc>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM ai_reviews WHERE pr_id = $1 AND created_at >= $2").bind(pr_id).bind(since).fetch_one(ex).await
}

/// Reviews of any PR in this repository since `since` (per-repo daily cap).
pub async fn count_for_repo_since<'e>(ex: impl PgExecutor<'e>, repo_id: i64, since: DateTime<Utc>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(DISTINCT r.id) FROM ai_reviews r JOIN pull_request_bounties l ON l.pr_id = r.pr_id JOIN bounties b ON b.id = l.bounty_id
         WHERE b.repo_id = $1 AND r.created_at >= $2",
    )
    .bind(repo_id)
    .bind(since)
    .fetch_one(ex)
    .await
}
