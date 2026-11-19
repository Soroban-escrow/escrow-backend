use crate::{models::User, new_id};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

const COLS: &str = "id, github_id, github_login, avatar_url, wallet, wallet_linked_at, is_admin, created_at, updated_at";

/// Creates the user on first sign-in and refreshes login/avatar/admin flag on every later one.
/// The admin flag follows configuration on each login, never what the browser says.
pub async fn upsert_github<'e>(
    ex: impl PgExecutor<'e>,
    github_id: i64,
    login: &str,
    avatar_url: Option<&str>,
    is_admin: bool,
) -> Result<User, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO users (id, github_id, github_login, avatar_url, is_admin) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (github_id) DO UPDATE SET github_login = EXCLUDED.github_login, avatar_url = EXCLUDED.avatar_url,
           is_admin = EXCLUDED.is_admin, updated_at = now()
         RETURNING {COLS}"
    )))
    .bind(new_id())
    .bind(github_id)
    .bind(login)
    .bind(avatar_url)
    .bind(is_admin)
    .fetch_one(ex)
    .await
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users WHERE id = $1"))).bind(id).fetch_optional(ex).await
}

pub async fn by_github_id<'e>(ex: impl PgExecutor<'e>, github_id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users WHERE github_id = $1"))).bind(github_id).fetch_optional(ex).await
}

pub async fn by_wallet<'e>(ex: impl PgExecutor<'e>, wallet: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users WHERE wallet = $1"))).bind(wallet).fetch_optional(ex).await
}

pub async fn by_login<'e>(ex: impl PgExecutor<'e>, login: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users WHERE lower(github_login) = lower($1) ORDER BY id LIMIT 1")))
        .bind(login)
        .fetch_optional(ex)
        .await
}

/// Fails with a unique violation when another account already holds the address.
pub async fn set_wallet<'e>(ex: impl PgExecutor<'e>, id: &str, address: &str, at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET wallet = $2, wallet_linked_at = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(address)
        .bind(at)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn clear_wallet<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET wallet = NULL, wallet_linked_at = NULL, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(ex)
        .await
        .map(|_| ())
}
