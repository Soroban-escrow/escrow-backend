//! Row types. JSON output is camelCase with string ids, matching what the web app already consumes.
//! Amounts are `NUMERIC(39,0)` in Postgres and travel as decimal strings; use the `*_i128` helpers for arithmetic.

use crate::serde_helpers::{i64_string, opt_i64_string};
use chrono::{DateTime, Utc};
use escrow_core::{BountyStatus, DisputeStatus, PayoutStatus};
use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    #[serde(serialize_with = "i64_string")]
    pub github_id: i64,
    pub github_login: String,
    pub avatar_url: Option<String>,
    pub wallet: Option<String>,
    pub wallet_linked_at: Option<DateTime<Utc>>,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct WalletChallenge {
    pub id: String,
    pub user_id: String,
    pub address: String,
    pub message: String,
    pub nonce: String,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    #[serde(serialize_with = "i64_string")]
    pub id: i64,
    pub account_login: String,
    pub account_type: String,
    pub suspended_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    #[serde(serialize_with = "i64_string")]
    pub id: i64,
    #[serde(serialize_with = "i64_string")]
    pub installation_id: i64,
    pub full_name: String,
    pub is_private: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct FundingIntent {
    pub id: String,
    pub repo_id: i64,
    pub issue_number: i32,
    pub requested_by: i64,
    pub token: String,
    pub amount: String,
    pub duration_days: i32,
    pub comment_id: i64,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl FundingIntent {
    pub fn amount_i128(&self) -> i128 {
        self.amount.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounty {
    #[serde(serialize_with = "i64_string")]
    pub id: i64,
    #[serde(serialize_with = "i64_string")]
    pub repo_id: i64,
    pub issue_number: i32,
    pub issue_title: String,
    pub issue_url: String,
    pub criteria: Vec<String>,
    pub maintainer: String,
    #[serde(serialize_with = "opt_i64_string")]
    pub maintainer_github_id: Option<i64>,
    pub token: String,
    pub amount: String,
    pub fee_bps: i32,
    pub deadline: DateTime<Utc>,
    pub relayer_allowed: bool,
    pub status: String,
    pub winner_address: Option<String>,
    #[serde(serialize_with = "opt_i64_string")]
    pub winner_github_id: Option<i64>,
    pub fund_tx: String,
    pub close_tx: Option<String>,
    #[serde(serialize_with = "opt_i64_string")]
    pub card_comment_id: Option<i64>,
    #[serde(skip)]
    pub last_event_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Bounty {
    pub fn status(&self) -> BountyStatus {
        self.status.parse().expect("bounties.status is constrained by a CHECK")
    }
    pub fn amount_i128(&self) -> i128 {
        self.amount.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    #[serde(serialize_with = "i64_string")]
    pub id: i64,
    pub number: i32,
    #[serde(serialize_with = "i64_string")]
    pub author_github_id: i64,
    pub author_login: String,
    pub head_sha: String,
    pub state: String,
    pub merged_at: Option<DateTime<Utc>>,
    pub diff_hash: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiReview {
    pub id: String,
    #[serde(serialize_with = "i64_string")]
    pub pr_id: i64,
    pub head_sha: String,
    pub model: String,
    pub verdict: String,
    pub score: i32,
    pub result: serde_json::Value,
    pub flags: Vec<String>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    #[serde(serialize_with = "opt_i64_string")]
    pub comment_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Payout {
    pub id: String,
    #[serde(serialize_with = "i64_string")]
    pub bounty_id: i64,
    pub pr_number: i32,
    #[serde(serialize_with = "i64_string")]
    pub winner_github_id: i64,
    pub winner_address: Option<String>,
    pub status: String,
    pub idempotency_key: String,
    pub tx_hash: Option<String>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Payout {
    pub fn status(&self) -> PayoutStatus {
        self.status.parse().expect("payouts.status is constrained by a CHECK")
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispute {
    pub id: String,
    #[serde(serialize_with = "i64_string")]
    pub bounty_id: i64,
    #[serde(serialize_with = "i64_string")]
    pub opened_by_github_id: i64,
    pub reason: String,
    pub evidence: serde_json::Value,
    pub status: String,
    pub resolution_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Dispute {
    pub fn status(&self) -> DisputeStatus {
        self.status.parse().expect("disputes.status is constrained by a CHECK")
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct IndexerCursor {
    pub id: String,
    pub cursor: Option<String>,
    pub ledger: i32,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuditEntry {
    pub id: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub run_at: DateTime<Utc>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub dedupe_key: Option<String>,
    pub last_error: Option<String>,
}

/// SQL column list for [`Bounty`]; the amount is cast to text so it decodes exactly.
pub const BOUNTY_COLS: &str = "id, repo_id, issue_number, issue_title, issue_url, criteria, maintainer, maintainer_github_id, token, \
     amount::text AS amount, fee_bps, deadline, relayer_allowed, status, winner_address, winner_github_id, fund_tx, close_tx, \
     card_comment_id, last_event_id, created_at, updated_at";

pub const INTENT_COLS: &str =
    "id, repo_id, issue_number, requested_by, token, amount::text AS amount, duration_days, comment_id, expires_at, used_at, created_at";
