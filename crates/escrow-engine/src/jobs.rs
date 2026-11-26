//! Job kinds and typed enqueue helpers. A job is enqueued in the same transaction as the change that
//! caused it, so a crash can never leave one without the other.

use crate::ports::ReviewScheduler;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use escrow_db::{Db, jobs};
use serde_json::json;
use sqlx::PgExecutor;

pub const WEBHOOK: &str = "webhook";
pub const CARD_REFRESH: &str = "card.refresh";
pub const PAYOUT_ANNOUNCE: &str = "payout.announce";
pub const DISPUTE_COMMENT: &str = "dispute.comment";
pub const REVIEW: &str = "review";
pub const ALL_KINDS: [&str; 5] = [WEBHOOK, CARD_REFRESH, PAYOUT_ANNOUNCE, DISPUTE_COMMENT, REVIEW];
/// Kinds that call GitHub (a webhook's commands reply with comments). Not claimed while the App is not configured.
pub const GITHUB_KINDS: [&str; 5] = [WEBHOOK, CARD_REFRESH, PAYOUT_ANNOUNCE, DISPUTE_COMMENT, REVIEW];

/// Queued jobs that need GitHub. While the App is not configured, these are waiting for it.
pub async fn count_waiting_for_github<'e>(ex: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status = 'queued' AND kind = ANY($1)")
        .bind(GITHUB_KINDS.map(String::from).to_vec())
        .fetch_one(ex)
        .await
}

pub async fn enqueue_webhook<'e>(
    ex: impl PgExecutor<'e>,
    delivery_id: &str,
    event: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    jobs::enqueue(
        ex,
        WEBHOOK,
        &json!({ "id": delivery_id, "event": event, "payload": payload }),
        Utc::now(),
        5,
        Some(&format!("webhook:{delivery_id}")),
    )
    .await
    .map(|_| ())
}

/// Refresh the GitHub card for a bounty. Many events for one bounty collapse into one queued refresh.
pub async fn enqueue_card_refresh<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<(), sqlx::Error> {
    jobs::enqueue(ex, CARD_REFRESH, &json!({ "bountyId": bounty_id }), Utc::now(), 8, Some(&format!("card:{bounty_id}"))).await.map(|_| ())
}

pub async fn enqueue_payout_announcement<'e>(ex: impl PgExecutor<'e>, bounty_id: i64) -> Result<(), sqlx::Error> {
    jobs::enqueue(ex, PAYOUT_ANNOUNCE, &json!({ "bountyId": bounty_id }), Utc::now(), 8, Some(&format!("announce:{bounty_id}")))
        .await
        .map(|_| ())
}

/// A GitHub comment about a dispute. `key` identifies the event/decision, so replaying the same event
/// cannot enqueue a second comment even if the first job is still waiting.
pub async fn enqueue_dispute_comment<'e>(
    ex: impl PgExecutor<'e>,
    bounty_id: i64,
    kind: &str,
    note: Option<&str>,
    key: &str,
) -> Result<(), sqlx::Error> {
    jobs::enqueue(
        ex,
        DISPUTE_COMMENT,
        &json!({ "bountyId": bounty_id, "kind": kind, "note": note, "key": key }),
        Utc::now(),
        8,
        Some(&format!("dispute-comment:{key}")),
    )
    .await
    .map(|_| ())
}

/// Debounced: a newer push to the same PR moves the waiting review later instead of adding another.
pub async fn enqueue_review<'e>(ex: impl PgExecutor<'e>, pr_id: i64, delay: std::time::Duration, force: bool) -> Result<(), sqlx::Error> {
    let run_at = Utc::now() + Duration::milliseconds(delay.as_millis() as i64);
    jobs::enqueue_debounced(ex, REVIEW, &json!({ "prId": pr_id, "force": force }), run_at, 3, &format!("review:{pr_id}")).await
}

/// Schedules reviews through the job queue.
pub struct JobReviewScheduler {
    pub db: Db,
}

#[async_trait]
impl ReviewScheduler for JobReviewScheduler {
    async fn schedule(&self, pr_id: i64, delay: std::time::Duration, force: bool) -> Result<(), String> {
        enqueue_review(&self.db.pool, pr_id, delay, force).await.map_err(|e| e.to_string())
    }
}
