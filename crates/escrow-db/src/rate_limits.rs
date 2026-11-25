//! Fixed-window rate-limit counters shared by every API instance. They live in Postgres, so no Redis is needed.

use sqlx::PgPool;

/// Counts one hit for `key` in the current fixed window of `window_secs` and returns that window's hit count.
/// It is a single atomic upsert, so concurrent instances never lose a count.
pub async fn hit(pool: &PgPool, key: &str, window_secs: i64) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO rate_limits (key, window_start, hits)
         VALUES ($1, to_timestamp((floor(extract(epoch FROM now()) / $2) * $2)::float8), 1)
         ON CONFLICT (key, window_start) DO UPDATE SET hits = rate_limits.hits + 1
         RETURNING hits",
    )
    .bind(key)
    .bind(window_secs)
    .fetch_one(pool)
    .await
}

/// Deletes windows that started more than `older_than_secs` ago. Returns how many rows were removed.
pub async fn prune(pool: &PgPool, older_than_secs: i64) -> Result<u64, sqlx::Error> {
    let done = sqlx::query("DELETE FROM rate_limits WHERE window_start < now() - make_interval(secs => $1)")
        .bind(older_than_secs as f64)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}
