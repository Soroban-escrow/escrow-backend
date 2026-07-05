//! Payout bookkeeping. One payout per bounty: the idempotency key `release:<bounty_id>` is unique, so a
//! second merged PR can never create a second payout. Every status change is a guarded UPDATE.

use crate::{is_unique_violation, models::Payout, new_id};
use serde::Serialize;
use sqlx::PgExecutor;

const COLS: &str = "id, bounty_id, pr_number, winner_github_id, winner_address, status, idempotency_key, tx_hash, attempts, last_error, created_at, updated_at";

pub fn idempotency_key(bounty_id: i64) -> String {
    format!("release:{bounty_id}")
}

/// `Ok(None)` means a payout for this bounty already exists (first merge wins).
pub async fn create_pending<'e>(
    ex: impl PgExecutor<'e>,
    bounty_id: i64,
    pr_number: i32,
    winner_github_id: i64,
) -> Result<Option<Payout>, sqlx::Error> {
    let r = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO payouts (id, bounty_id, pr_number, winner_github_id, status, idempotency_key) VALUES ($1,$2,$3,$4,'PENDING_WALLET',$5) RETURNING {COLS}"
    )))
    .bind(new_id())
    .bind(bounty_id)
    .bind(pr_number)
    .bind(winner_github_id)
    .bind(idempotency_key(bounty_id))
    .fetch_one(ex)
    .await;
    match r {
        Ok(p) => Ok(Some(p)),
        Err(e) if is_unique_violation(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn for_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<Option<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM payouts WHERE idempotency_key = $1")))
        .bind(idempotency_key(bounty_id))
        .fetch_optional(ex)
        .await
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM payouts WHERE id = $1"))).bind(id).fetch_optional(ex).await
}

pub async fn all_for_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<Vec<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM payouts WHERE bounty_id = $1 ORDER BY created_at")))
        .bind(bounty_id)
        .fetch_all(ex)
        .await
}

/// Moves PENDING_WALLET forward. Guarded: of several concurrent callers exactly one gets `true`.
pub async fn release_from_pending<'e>(ex: impl PgExecutor<'e>, id: &str, status: &str, address: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE payouts SET status = $2, winner_address = $3, updated_at = now() WHERE id = $1 AND status = 'PENDING_WALLET'")
        .bind(id)
        .bind(status)
        .bind(address)
        .execute(ex)
        .await
        .map(|r| r.rows_affected() == 1)
}

pub async fn mark_failed<'e>(ex: impl PgExecutor<'e>, id: &str, error: &str, bump_attempts: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payouts SET status = 'FAILED', last_error = $2, attempts = attempts + $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(error)
        .bind(i32::from(bump_attempts))
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn mark_confirmed<'e>(ex: impl PgExecutor<'e>, id: &str, tx_hash: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE payouts SET status = 'CONFIRMED', tx_hash = COALESCE($2, tx_hash), last_error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(tx_hash)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Records the submitted hash BEFORE anyone waits on it, so a crash can never lead to a second submission.
pub async fn mark_submitted<'e>(ex: impl PgExecutor<'e>, id: &str, tx_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payouts SET status = 'SUBMITTED', tx_hash = $2, attempts = attempts + 1, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(tx_hash)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn record_attempt_error<'e>(ex: impl PgExecutor<'e>, id: &str, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payouts SET last_error = $2, attempts = attempts + 1, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(ex)
        .await
        .map(|_| ())
}

/// A submitted transaction that provably cannot land any more: forget its hash and submit afresh.
pub async fn requeue<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payouts SET status = 'QUEUED', tx_hash = NULL, attempts = 0, last_error = NULL, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(ex)
        .await
        .map(|_| ())
}

/// Admin retry: only a FAILED payout with no submitted transaction. Returns whether it was re-queued.
pub async fn retry_failed<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE payouts SET status = 'QUEUED', attempts = 0, last_error = NULL, updated_at = now() WHERE id = $1 AND status = 'FAILED' AND tx_hash IS NULL")
        .bind(id)
        .execute(ex)
        .await
        .map(|r| r.rows_affected() == 1)
}

/// The indexer saw the release on chain: whatever state we were in, the payout is done.
pub async fn confirm_from_chain<'e>(ex: impl PgExecutor<'e>, bounty_id: i64, tx_hash: &str, winner: &str) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE payouts SET status = 'CONFIRMED', tx_hash = $2, winner_address = $3, updated_at = now()
         WHERE bounty_id = $1 AND status IN ('QUEUED','SUBMITTED','AWAITING_MAINTAINER')",
    )
    .bind(bounty_id)
    .bind(tx_hash)
    .bind(winner)
    .execute(ex)
    .await
    .map(|r| r.rows_affected())
}

/// Payouts the relayer should work on, oldest first.
pub async fn due<'e>(ex: impl PgExecutor<'e>, limit: i64) -> Result<Vec<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM payouts WHERE status IN ('QUEUED','SUBMITTED') ORDER BY created_at LIMIT $1"
    )))
    .bind(limit)
    .fetch_all(ex)
    .await
}

pub async fn by_status<'e>(ex: impl PgExecutor<'e>, status: &str, limit: i64) -> Result<Vec<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM payouts WHERE status = $1 ORDER BY updated_at DESC LIMIT $2")))
        .bind(status)
        .bind(limit)
        .fetch_all(ex)
        .await
}

pub async fn pending_for_winner<'e>(ex: impl PgExecutor<'e>, github_id: i64) -> Result<Vec<Payout>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM payouts WHERE winner_github_id = $1 AND status = 'PENDING_WALLET' ORDER BY created_at"
    )))
    .bind(github_id)
    .fetch_all(ex)
    .await
}

pub async fn count_by_status<'e>(ex: impl PgExecutor<'e>, statuses: &[&str]) -> Result<i64, sqlx::Error> {
    let s: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
    sqlx::query_scalar("SELECT count(*) FROM payouts WHERE status = ANY($1)").bind(s).fetch_one(ex).await
}

/// A payout with the bounty facts a contributor's earnings page needs.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Earning {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub payout: Payout,
    #[sqlx(skip)]
    pub bounty: Option<EarningBounty>,
    #[serde(skip)]
    pub b_token: String,
    #[serde(skip)]
    pub b_amount: String,
    #[serde(skip)]
    pub b_fee_bps: i32,
    #[serde(skip)]
    pub b_title: String,
    #[serde(skip)]
    pub b_id: i64,
    #[serde(skip)]
    pub b_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EarningBounty {
    #[serde(serialize_with = "crate::serde_helpers::i64_string")]
    pub id: i64,
    pub issue_url: String,
    pub token: String,
    pub amount: String,
    pub fee_bps: i32,
    pub issue_title: String,
}

pub async fn earnings<'e>(ex: impl PgExecutor<'e>, github_id: i64, only_confirmed: bool) -> Result<Vec<Earning>, sqlx::Error> {
    let mut rows: Vec<Earning> = sqlx::query_as(
        "SELECT p.id, p.bounty_id, p.pr_number, p.winner_github_id, p.winner_address, p.status, p.idempotency_key, p.tx_hash, p.attempts,
                p.last_error, p.created_at, p.updated_at,
                b.token AS b_token, b.amount::text AS b_amount, b.fee_bps AS b_fee_bps, b.issue_title AS b_title, b.id AS b_id, b.issue_url AS b_url
         FROM payouts p JOIN bounties b ON b.id = p.bounty_id
         WHERE p.winner_github_id = $1 AND ($2 = FALSE OR p.status = 'CONFIRMED') ORDER BY p.created_at DESC",
    )
    .bind(github_id)
    .bind(only_confirmed)
    .fetch_all(ex)
    .await?;
    for r in &mut rows {
        r.bounty = Some(EarningBounty {
            id: r.b_id,
            issue_url: r.b_url.clone(),
            token: r.b_token.clone(),
            amount: r.b_amount.clone(),
            fee_bps: r.b_fee_bps,
            issue_title: r.b_title.clone(),
        });
    }
    Ok(rows)
}
