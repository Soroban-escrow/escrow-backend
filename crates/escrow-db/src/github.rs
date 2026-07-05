//! GitHub App installations and the repositories they cover.

use crate::models::{Installation, Repo};
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

pub async fn upsert_installation<'e>(
    ex: impl PgExecutor<'e>,
    id: i64,
    account_login: &str,
    account_type: &str,
    suspended_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO installations (id, account_login, account_type, suspended_at) VALUES ($1,$2,$3,$4)
         ON CONFLICT (id) DO UPDATE SET account_login = EXCLUDED.account_login, account_type = EXCLUDED.account_type, suspended_at = EXCLUDED.suspended_at",
    )
    .bind(id)
    .bind(account_login)
    .bind(account_type)
    .bind(suspended_at)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Registers an installation seen only through another event, without touching an existing row.
pub async fn ensure_installation<'e>(ex: impl PgExecutor<'e>, id: i64, account_login: &str, account_type: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO installations (id, account_login, account_type) VALUES ($1,$2,$3) ON CONFLICT (id) DO NOTHING")
        .bind(id)
        .bind(account_login)
        .bind(account_type)
        .execute(ex)
        .await
        .map(|_| ())
}

/// Suspends rather than deletes: bounties still point at the rows.
pub async fn suspend_installation<'e>(ex: impl PgExecutor<'e>, id: i64, at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE installations SET suspended_at = $2 WHERE id = $1").bind(id).bind(at).execute(ex).await.map(|_| ())
}

pub async fn unsuspend_installation<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE installations SET suspended_at = NULL WHERE id = $1").bind(id).execute(ex).await.map(|_| ())
}

pub async fn get_installation<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<Installation>, sqlx::Error> {
    sqlx::query_as("SELECT id, account_login, account_type, suspended_at, created_at FROM installations WHERE id = $1")
        .bind(id)
        .fetch_optional(ex)
        .await
}

pub async fn upsert_repo<'e>(
    ex: impl PgExecutor<'e>,
    id: i64,
    installation_id: i64,
    full_name: &str,
    is_private: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repos (id, installation_id, full_name, is_private) VALUES ($1,$2,$3,$4)
         ON CONFLICT (id) DO UPDATE SET installation_id = EXCLUDED.installation_id, full_name = EXCLUDED.full_name, is_private = EXCLUDED.is_private",
    )
    .bind(id)
    .bind(installation_id)
    .bind(full_name)
    .bind(is_private)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn get_repo<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private FROM repos WHERE id = $1").bind(id).fetch_optional(ex).await
}

pub async fn list_repos<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private FROM repos ORDER BY full_name").fetch_all(ex).await
}
