//! Pull requests and their links to bounties.

use crate::models::{AiReview, Bounty, PullRequest};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

const COLS: &str = "id, number, author_github_id, author_login, head_sha, state, merged_at, diff_hash";

pub struct PrUpsert<'a> {
    pub id: i64,
    pub number: i32,
    pub author_github_id: i64,
    pub author_login: &'a str,
    pub head_sha: &'a str,
    pub state: &'a str,
    pub merged_at: Option<DateTime<Utc>>,
}

pub async fn upsert<'e>(ex: impl PgExecutor<'e>, p: &PrUpsert<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pull_requests (id, number, author_github_id, author_login, head_sha, state, merged_at) VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (id) DO UPDATE SET number = EXCLUDED.number, author_github_id = EXCLUDED.author_github_id,
           author_login = EXCLUDED.author_login, head_sha = EXCLUDED.head_sha, state = EXCLUDED.state, merged_at = EXCLUDED.merged_at",
    )
    .bind(p.id)
    .bind(p.number)
    .bind(p.author_github_id)
    .bind(p.author_login)
    .bind(p.head_sha)
    .bind(p.state)
    .bind(p.merged_at)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<PullRequest>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM pull_requests WHERE id = $1"))).bind(id).fetch_optional(ex).await
}

/// Links a PR to a bounty it closes. Idempotent.
pub async fn link<'e>(ex: impl PgExecutor<'e>, pr_id: i64, bounty_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO pull_request_bounties (pr_id, bounty_id) VALUES ($1,$2) ON CONFLICT DO NOTHING")
        .bind(pr_id)
        .bind(bounty_id)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn for_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<Vec<PullRequest>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.id, p.number, p.author_github_id, p.author_login, p.head_sha, p.state, p.merged_at, p.diff_hash
         FROM pull_requests p JOIN pull_request_bounties l ON l.pr_id = p.id WHERE l.bounty_id = $1 ORDER BY p.id",
    )
    .bind(bounty_id)
    .fetch_all(ex)
    .await
}

/// Bounties a PR closes that are still live, oldest first.
pub async fn live_bounties<'e>(ex: impl PgExecutor<'e>, pr_id: i64) -> Result<Vec<Bounty>, sqlx::Error> {
    let cols = crate::bounties::qualified_cols();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {cols} FROM bounties b JOIN pull_request_bounties l ON l.bounty_id = b.id
         WHERE l.pr_id = $1 AND b.status IN ('OPEN','DISPUTED') ORDER BY b.id"
    )))
    .bind(pr_id)
    .fetch_all(ex)
    .await
}

/// A PR in this repository by its number, if it is linked to any of our bounties.
pub async fn find_linked_by_number<'e>(ex: impl PgExecutor<'e>, repo_id: i64, number: i32) -> Result<Option<PullRequest>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.id, p.number, p.author_github_id, p.author_login, p.head_sha, p.state, p.merged_at, p.diff_hash
         FROM pull_requests p JOIN pull_request_bounties l ON l.pr_id = p.id JOIN bounties b ON b.id = l.bounty_id
         WHERE p.number = $2 AND b.repo_id = $1 LIMIT 1",
    )
    .bind(repo_id)
    .bind(number)
    .fetch_optional(ex)
    .await
}

pub async fn set_diff(pool: impl PgExecutor<'_>, id: i64, hash: &str, head_sha: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE pull_requests SET diff_hash = $2, head_sha = $3 WHERE id = $1")
        .bind(id)
        .bind(hash)
        .bind(head_sha)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Another PR on the same bounty whose change set hashes the same: a likely duplicate submission.
pub async fn has_duplicate_diff<'e>(ex: impl PgExecutor<'e>, pr_id: i64, bounty_id: i64, hash: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pull_requests p JOIN pull_request_bounties l ON l.pr_id = p.id
                        WHERE l.bounty_id = $2 AND p.id <> $1 AND p.diff_hash = $3)",
    )
    .bind(pr_id)
    .bind(bounty_id)
    .bind(hash)
    .fetch_one(ex)
    .await
}

/// Open PRs on a bounty with their newest review (for ranking).
pub async fn open_with_latest_review<'e>(
    ex: impl PgExecutor<'e>,
    bounty_id: i64,
) -> Result<Vec<(PullRequest, Option<AiReview>)>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        pr: PullRequest,
        r_id: Option<String>,
        r_head_sha: Option<String>,
        r_model: Option<String>,
        r_verdict: Option<String>,
        r_score: Option<i32>,
        r_result: Option<serde_json::Value>,
        r_flags: Option<Vec<String>>,
        r_input: Option<i32>,
        r_output: Option<i32>,
        r_comment: Option<i64>,
        r_created: Option<DateTime<Utc>>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT p.id, p.number, p.author_github_id, p.author_login, p.head_sha, p.state, p.merged_at, p.diff_hash,
                r.id AS r_id, r.head_sha AS r_head_sha, r.model AS r_model, r.verdict AS r_verdict, r.score AS r_score, r.result AS r_result,
                r.flags AS r_flags, r.input_tokens AS r_input, r.output_tokens AS r_output, r.comment_id AS r_comment, r.created_at AS r_created
         FROM pull_requests p JOIN pull_request_bounties l ON l.pr_id = p.id
         LEFT JOIN LATERAL (SELECT * FROM ai_reviews x WHERE x.pr_id = p.id ORDER BY x.created_at DESC LIMIT 1) r ON TRUE
         WHERE l.bounty_id = $1 AND p.state = 'open' ORDER BY p.id",
    )
    .bind(bounty_id)
    .fetch_all(ex)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let review = r.r_id.map(|id| AiReview {
                id,
                pr_id: r.pr.id,
                head_sha: r.r_head_sha.unwrap_or_default(),
                model: r.r_model.unwrap_or_default(),
                verdict: r.r_verdict.unwrap_or_default(),
                score: r.r_score.unwrap_or(0),
                result: r.r_result.unwrap_or(serde_json::Value::Null),
                flags: r.r_flags.unwrap_or_default(),
                input_tokens: r.r_input.unwrap_or(0),
                output_tokens: r.r_output.unwrap_or(0),
                comment_id: r.r_comment,
                created_at: r.r_created.unwrap_or_else(Utc::now),
            });
            (r.pr, review)
        })
        .collect())
}
