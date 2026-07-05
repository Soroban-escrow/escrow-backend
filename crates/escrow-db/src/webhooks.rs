//! Webhook delivery de-duplication and the indexer's resume point.

use crate::models::IndexerCursor;
use sqlx::PgExecutor;

/// `true` for a delivery we have not seen; `false` for a redelivery (GitHub retries).
pub async fn insert_delivery<'e>(ex: impl PgExecutor<'e>, id: &str, event: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("INSERT INTO webhook_deliveries (id, event) VALUES ($1,$2) ON CONFLICT (id) DO NOTHING")
        .bind(id)
        .bind(event)
        .execute(ex)
        .await
        .map(|r| r.rows_affected() == 1)
}

pub async fn mark_processed<'e>(ex: impl PgExecutor<'e>, id: &str, error: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE webhook_deliveries SET processed_at = now(), error = $2 WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn get_cursor<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<Option<IndexerCursor>, sqlx::Error> {
    sqlx::query_as("SELECT id, cursor, ledger, updated_at FROM indexer_cursors WHERE id = $1").bind(id).fetch_optional(ex).await
}

pub async fn save_cursor<'e>(ex: impl PgExecutor<'e>, id: &str, cursor: &str, ledger: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO indexer_cursors (id, cursor, ledger) VALUES ($1,$2,$3)
         ON CONFLICT (id) DO UPDATE SET cursor = EXCLUDED.cursor, ledger = EXCLUDED.ledger, updated_at = now()",
    )
    .bind(id)
    .bind(cursor)
    .bind(ledger)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn delete_cursor<'e>(ex: impl PgExecutor<'e>, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM indexer_cursors WHERE id = $1").bind(id).execute(ex).await.map(|_| ())
}
