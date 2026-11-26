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

/// Finishes a statement whose `up` CTE returns the repo's `id, full_name`: rewrites every bounty of that repo
/// whose `issue_url` is not `https://github.com/{full_name}/issues/{n}` (the form the indexer stores), so a
/// rename or transfer is reflected everywhere `issue_url` is read. Unchanged URLs are not touched.
const REWRITE_ISSUE_URLS: &str = "
    UPDATE bounties b SET issue_url = 'https://github.com/' || up.full_name || '/issues/' || b.issue_number
    FROM up
    WHERE b.repo_id = up.id AND b.issue_url IS DISTINCT FROM 'https://github.com/' || up.full_name || '/issues/' || b.issue_number";

/// Inserts or updates a repository. When the name changed (a rename or transfer seen through any event,
/// or `sync-installations`), the stored issue URLs of its bounties are rewritten in the same statement.
pub async fn upsert_repo<'e>(
    ex: impl PgExecutor<'e>,
    id: i64,
    installation_id: i64,
    full_name: &str,
    is_private: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH up AS (
            INSERT INTO repos (id, installation_id, full_name, is_private) VALUES ($1,$2,$3,$4)
            ON CONFLICT (id) DO UPDATE SET installation_id = EXCLUDED.installation_id, full_name = EXCLUDED.full_name, is_private = EXCLUDED.is_private
            RETURNING id, full_name
         ) {REWRITE_ISSUE_URLS}"
    )))
    .bind(id)
    .bind(installation_id)
    .bind(full_name)
    .bind(is_private)
    .execute(ex)
    .await
    .map(|_| ())
}

/// Renames a known repository (nothing happens for an unknown id) and rewrites its bounties' issue URLs.
/// Returns how many bounties got a new URL.
pub async fn rename_repo<'e>(ex: impl PgExecutor<'e>, id: i64, full_name: &str) -> Result<u64, sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH up AS (UPDATE repos SET full_name = $2 WHERE id = $1 RETURNING id, full_name) {REWRITE_ISSUE_URLS}"
    )))
    .bind(id)
    .bind(full_name)
    .execute(ex)
    .await
    .map(|r| r.rows_affected())
}

/// Marks a repository as deleted/removed from the installation (soft delete to preserve bounty references).
pub async fn suspend_repo<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<(), sqlx::Error> {
    // Soft delete: we could add a 'suspended_at' column, but for now just removing from active repos
    // is enough to prevent new bounties. Existing bounties remain intact.
    sqlx::query("UPDATE repos SET installation_id = -1 WHERE id = $1")
        .bind(id)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn get_repo<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private, suspended_at FROM repos WHERE id = $1")
        .bind(id)
        .fetch_optional(ex)
        .await
}

pub async fn list_repos<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<Repo>, sqlx::Error> {
    sqlx::query_as("SELECT id, installation_id, full_name, is_private, suspended_at FROM repos ORDER BY full_name").fetch_all(ex).await
}

/// Whether AI reviews are on for a repository (on unless a `repo_settings` row turns them off).
pub async fn ai_reviews_enabled<'e>(ex: impl PgExecutor<'e>, repo_id: i64) -> Result<bool, sqlx::Error> {
    let v: Option<bool> =
        sqlx::query_scalar("SELECT ai_reviews FROM repo_settings WHERE repo_id = $1").bind(repo_id).fetch_optional(ex).await?;
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
