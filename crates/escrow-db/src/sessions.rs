//! Browser sessions and wallet-link challenges.

use crate::models::{User, WalletChallenge};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// `hashed_id` is the SHA-256 of the cookie value; the cookie value itself is never stored.
pub async fn create<'e>(ex: impl PgExecutor<'e>, hashed_id: &str, user_id: &str, expires_at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(hashed_id)
        .bind(user_id)
        .bind(expires_at)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn user_for<'e>(ex: impl PgExecutor<'e>, hashed_id: &str, now: DateTime<Utc>) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(
        "SELECT u.id, u.github_id, u.github_login, u.avatar_url, u.wallet, u.wallet_linked_at, u.is_admin, u.created_at, u.updated_at
         FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.id = $1 AND s.expires_at > $2",
    )
    .bind(hashed_id)
    .bind(now)
    .fetch_optional(ex)
    .await
}

pub async fn delete<'e>(ex: impl PgExecutor<'e>, hashed_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE id = $1").bind(hashed_id).execute(ex).await.map(|_| ())
}

pub async fn purge_expired<'e>(ex: impl PgExecutor<'e>, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= $1").bind(now).execute(ex).await.map(|r| r.rows_affected())
}

pub async fn create_challenge<'e>(
    ex: impl PgExecutor<'e>,
    id: &str,
    user_id: &str,
    address: &str,
    message: &str,
    nonce: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO wallet_challenges (id, user_id, address, message, nonce, expires_at) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(id)
        .bind(user_id)
        .bind(address)
        .bind(message)
        .bind(nonce)
        .bind(expires_at)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn get_challenge<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<WalletChallenge>, sqlx::Error> {
    sqlx::query_as("SELECT id, user_id, address, message, nonce, expires_at, used_at FROM wallet_challenges WHERE id = $1")
        .bind(id)
        .fetch_optional(ex)
        .await
}

/// Atomically marks a challenge used. Of several simultaneous verifications exactly one gets `true`.
pub async fn burn_challenge<'e>(ex: impl PgExecutor<'e>, id: &str, at: DateTime<Utc>) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE wallet_challenges SET used_at = $2 WHERE id = $1 AND used_at IS NULL")
        .bind(id)
        .bind(at)
        .execute(ex)
        .await
        .map(|r| r.rows_affected() == 1)
}

pub async fn purge_challenges<'e>(ex: impl PgExecutor<'e>, before: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM wallet_challenges WHERE expires_at <= $1").bind(before).execute(ex).await.map(|r| r.rows_affected())
}
