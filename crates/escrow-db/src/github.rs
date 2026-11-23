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
         ON CONFLICT (id) DO UPDATE SET installation_id = EXCLUDED.installation_id, full_name = EXCLUDED.full_name, is_private = EXCLUDED.is_private, removed_at = NULL",
    )
    .bind(id)
    .bind(installation_id)
    .bind(full_name)
    .bind(is_private)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Marks a repository as removed from its installation. The row stays (bounties reference it);
/// `removed_at` blocks new bounty creation until the repo is re-added.
pub async fn suspend_repo<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE repos SET removed_at = now() WHERE id = $1").bind(id).execute(ex).await.map(|_| ())
}

/// Whether the repository was removed from its installation.
pub async fn repo_removed<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<bool, sqlx::Error> {
    let v: Option<bool> = sqlx::query_scalar("SELECT removed_at IS NOT NULL FROM repos WHERE id = $1").bind(id).fetch_optional(ex).await?;
    Ok(v.unwrap_or(false))
}

pub async fn get_repo<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private FROM repos WHERE id = $1").bind(id).fetch_optional(ex).await
}

pub async fn repos_by_ids<'e>(ex: impl PgExecutor<'e>, ids: &[i64]) -> Result<Vec<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private FROM repos WHERE id = ANY($1)")
        .bind(ids)
        .fetch_all(ex)
        .await
}

pub async fn list_repos<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private FROM repos ORDER BY full_name").fetch_all(ex).await
}

/// Whether AI reviews are on for a repository (on unless a `repo_settings` row turns them off).
pub async fn ai_reviews_enabled<'e>(ex: impl PgExecutor<'e>, repo_id: i64) -> Result<bool, sqlx::Error> {
    let v: Option<bool> = sqlx::query_scalar("SELECT ai_reviews FROM repo_settings WHERE repo_id = $1").bind(repo_id).fetch_optional(ex).await?;
    Ok(v.unwrap_or(true))
}

/// Turns AI reviews on or off for a repository.
pub async fn set_ai_reviews<'e>(ex: impl PgExecutor<'e>, repo_id: i64, enabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repo_settings (repo_id, ai_reviews) VALUES ($1, $2)
         ON CONFLICT (repo_id) DO UPDATE SET ai_reviews = EXCLUDED.ai_reviews",
    )
    .bind(repo_id)
    .bind(enabled)
    .execute(ex)
    .await?;
    Ok(())
}
