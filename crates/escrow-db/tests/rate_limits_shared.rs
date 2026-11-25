//! Two handles on one database share one budget: the count is taken in Postgres, not in process memory.
//! `#[sqlx::test]` creates a fresh database per test.

use escrow_db::{Db, MIGRATOR, rate_limits};
use sqlx::PgPool;

#[sqlx::test(migrator = "MIGRATOR")]
async fn two_handles_share_one_count(pool: PgPool) {
    let instance_a = Db::from_pool(pool.clone());
    let instance_b = Db::from_pool(pool.clone());
    assert_eq!(rate_limits::hit(&instance_a.pool, "m:user-1", 60).await.unwrap(), 1);
    assert_eq!(rate_limits::hit(&instance_b.pool, "m:user-1", 60).await.unwrap(), 2, "the second instance sees the first one's hit");
    assert_eq!(rate_limits::hit(&instance_a.pool, "m:user-1", 60).await.unwrap(), 3);
}
