//! Shared rate-limit counters against a real Postgres. `#[sqlx::test]` creates a fresh database per test.

use escrow_db::{MIGRATOR, rate_limits};
use sqlx::PgPool;

#[sqlx::test(migrator = "MIGRATOR")]
async fn counts_hits_per_key_within_a_window(pool: PgPool) {
    assert_eq!(rate_limits::hit(&pool, "g:a", 60).await.unwrap(), 1);
    assert_eq!(rate_limits::hit(&pool, "g:a", 60).await.unwrap(), 2);
    assert_eq!(rate_limits::hit(&pool, "g:b", 60).await.unwrap(), 1, "keys are independent");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn prune_removes_only_old_windows(pool: PgPool) {
    rate_limits::hit(&pool, "g:a", 60).await.unwrap();
    assert_eq!(rate_limits::prune(&pool, 3600).await.unwrap(), 0, "a fresh window is kept");
    sqlx::query("UPDATE rate_limits SET window_start = now() - interval '2 hours'").execute(&pool).await.unwrap();
    assert_eq!(rate_limits::prune(&pool, 3600).await.unwrap(), 1);
}
