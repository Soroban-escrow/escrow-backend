//! Untracked funded events: Funded events for repositories that don't have a row yet.
//! When the repo is later installed, these events can be applied.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UntrackedFundedEvent {
    pub id: String,
    pub event_id: String,
    pub bounty_id: i64,
    pub repo_id: i64,
    pub issue_number: i32,
    pub maintainer: String,
    pub token: String,
    pub amount: String,
    pub fee_bps: i32,
    pub deadline: DateTime<Utc>,
    pub relayer_allowed: bool,
    pub ledger: i32,
    pub tx_hash: String,
    pub closed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl UntrackedFundedEvent {
    pub fn amount_i128(&self) -> i128 {
        self.amount.parse().unwrap_or(0)
    }
}

/// Stores a Funded event for a repo that doesn't exist yet.
pub async fn store_untracked_funded<'e>(
    ex: impl PgExecutor<'e>,
    event_id: &str,
    bounty_id: u64,
    repo_id: u64,
    issue_number: u64,
    maintainer: &str,
    token: &str,
    amount: i128,
    fee_bps: u32,
    deadline: DateTime<Utc>,
    relayer_allowed: bool,
    ledger: u32,
    tx_hash: &str,
    closed_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let id = crate::new_id();
    sqlx::query(
        r#"
        INSERT INTO untracked_funded_events (id, event_id, bounty_id, repo_id, issue_number, maintainer, token, amount, fee_bps, deadline, relayer_allowed, ledger, tx_hash, closed_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8::numeric,$9,$10,$11,$12,$13,$14)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(id)
    .bind(event_id)
    .bind(i64::try_from(bounty_id).unwrap_or(i64::MAX))
    .bind(i64::try_from(repo_id).unwrap_or(0))
    .bind(i32::try_from(issue_number).unwrap_or(0))
    .bind(maintainer)
    .bind(token)
    .bind(amount.to_string())
    .bind(i32::try_from(fee_bps).unwrap_or(0))
    .bind(deadline)
    .bind(relayer_allowed)
    .bind(i32::try_from(ledger).unwrap_or(i32::MAX))
    .bind(tx_hash)
    .bind(closed_at)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Retrieves all untracked funded events for a given repo.
pub async fn get_untracked_for_repo<'e>(ex: impl PgExecutor<'e>, repo_id: i64) -> Result<Vec<UntrackedFundedEvent>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, event_id, bounty_id, repo_id, issue_number, maintainer, token, amount::text AS amount, fee_bps, deadline, relayer_allowed, ledger, tx_hash, closed_at, created_at
         FROM untracked_funded_events WHERE repo_id = $1 ORDER BY ledger",
    )
    .bind(repo_id)
    .fetch_all(ex)
    .await
}

/// Deletes untracked funded events that have been applied (by bounty_id).
pub async fn delete_untracked_by_bounty<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM untracked_funded_events WHERE bounty_id = $1").bind(bounty_id).execute(ex).await?;
    Ok(res.rows_affected())
}

/// Gets the minimum ledger from untracked events for a repo, to tell the operator where to reindex from.
pub async fn min_untracked_ledger_for_repo<'e>(ex: impl PgExecutor<'e>, repo_id: i64) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar("SELECT MIN(ledger) FROM untracked_funded_events WHERE repo_id = $1").bind(repo_id).fetch_optional(ex).await
}
