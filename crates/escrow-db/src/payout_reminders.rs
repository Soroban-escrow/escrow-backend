//! Wallet reminders for merged PRs whose payout is waiting for the contributor's wallet.
//! `payout_reminders` holds one row per (payout, stage), so each reminder is sent at most once, even across restarts.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// A payout that is waiting for a wallet, with what a reminder needs.
#[derive(Debug, sqlx::FromRow)]
pub struct WaitingPayout {
    pub payout_id: String,
    pub bounty_id: i64,
    pub pr_number: i32,
    pub created_at: DateTime<Utc>,
}

/// Payouts still in `PENDING_WALLET`, oldest first.
pub async fn waiting<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<WaitingPayout>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id AS payout_id, bounty_id, pr_number, created_at FROM payouts
         WHERE status = 'PENDING_WALLET' ORDER BY created_at LIMIT 500",
    )
    .fetch_all(ex)
    .await
}

/// Payouts that were reminded and have since been paid to a wallet, and have not had the one-time confirmation yet.
pub async fn linked_after_reminder<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<WaitingPayout>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.id AS payout_id, p.bounty_id, p.pr_number, p.created_at FROM payouts p
         WHERE p.status <> 'PENDING_WALLET'
           AND EXISTS (SELECT 1 FROM payout_reminders r WHERE r.payout_id = p.id AND r.stage IN ('h1','h24','h72'))
           AND NOT EXISTS (SELECT 1 FROM payout_reminders r WHERE r.payout_id = p.id AND r.stage = 'linked')
         LIMIT 500",
    )
    .fetch_all(ex)
    .await
}

/// The stages already recorded for a payout (`h1`, `h24`, `h72`, `linked`, `skipped`).
pub async fn stages_sent<'e>(ex: impl PgExecutor<'e>, payout_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT stage FROM payout_reminders WHERE payout_id = $1").bind(payout_id).fetch_all(ex).await
}

/// Records a stage. Returns `false` if it was already recorded (a concurrent run got there first).
pub async fn record<'e>(ex: impl PgExecutor<'e>, payout_id: &str, stage: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("INSERT INTO payout_reminders (payout_id, stage) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(payout_id)
        .bind(stage)
        .execute(ex)
        .await
        .map(|r| r.rows_affected() == 1)
}

/// Whether reminders are on for a repository (on unless a `repo_settings` row turns them off).
pub async fn wallet_reminders_enabled<'e>(ex: impl PgExecutor<'e>, repo_id: i64) -> Result<bool, sqlx::Error> {
    let v: Option<bool> = sqlx::query_scalar("SELECT wallet_reminders FROM repo_settings WHERE repo_id = $1").bind(repo_id).fetch_optional(ex).await?;
    Ok(v.unwrap_or(true))
}
