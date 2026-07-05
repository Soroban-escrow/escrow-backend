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
pub const REVIEW: &str = "review";
pub const ALL_KINDS: [&str; 4] = [WEBHOOK, CARD_REFRESH, PAYOUT_ANNOUNCE, REVIEW];

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
