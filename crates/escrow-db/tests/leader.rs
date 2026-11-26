//! Single-writer invariant tests for `leader::try_acquire` (#110, #111).

use escrow_db::{MIGRATOR, leader};
use sqlx::PgPool;

/// With one lock id, two acquisitions cannot both succeed.
#[sqlx::test(migrator = "MIGRATOR")]
async fn one_acquire_at_a_time(pool: PgPool) {
    let first = leader::try_acquire(&pool, 42).await.unwrap().expect("first acquire wins");
    let second = leader::try_acquire(&pool, 42).await.unwrap();
    assert!(second.is_none(), "a second acquire on the same lock id must return None");
    assert_eq!(first.lock_id(), 42);
}

/// Dropping the guard releases the lock so a later acquire succeeds. This is how a crashed
/// leader hands over automatically when its connection dies - the test models the clean case.
#[sqlx::test(migrator = "MIGRATOR")]
async fn releases_on_drop_so_a_standby_can_take_over(pool: PgPool) {
    let first = leader::try_acquire(&pool, 7).await.unwrap().expect("first acquire wins");
    drop(first);
    let took_over = leader::try_acquire(&pool, 7).await.unwrap();
    assert!(took_over.is_some(), "after drop the lock must be free for the standby");
}

/// Distinct lock ids do not interfere - the relayer and indexer run concurrently in
/// production, so their locks must be independent.
#[sqlx::test(migrator = "MIGRATOR")]
async fn relayer_and_indexer_locks_are_independent(pool: PgPool) {
    let r = leader::try_acquire(&pool, leader::RELAYER_LOCK_ID).await.unwrap().expect("relayer lock free");
    let i = leader::try_acquire(&pool, leader::INDEXER_LOCK_ID).await.unwrap().expect("indexer lock free");
    assert_ne!(r.lock_id(), i.lock_id());
}

/// Keepalive on a live leader works; this is what the relayer/indexer loops use to tell when
/// the connection has died.
#[sqlx::test(migrator = "MIGRATOR")]
async fn ping_succeeds_while_leader(pool: PgPool) {
    let mut guard = leader::try_acquire(&pool, 1234).await.unwrap().unwrap();
    for _ in 0..3 {
        guard.ping().await.expect("ping should succeed while holding");
    }
}

/// Only one of N concurrent callers acquires. The real relayer/indexer are long-running loops;
/// this test fast-forwards the race and asserts the single-writer property directly.
#[sqlx::test(migrator = "MIGRATOR")]
async fn concurrent_acquires_pick_exactly_one_winner(pool: PgPool) {
    const N: usize = 8;
    let mut tasks = Vec::with_capacity(N);
    for _ in 0..N {
        let p = pool.clone();
        tasks.push(tokio::spawn(async move { leader::try_acquire(&p, 999).await.unwrap() }));
    }
    let mut wins = 0;
    let mut guards = Vec::new();
    for t in tasks {
        if let Some(g) = t.await.unwrap() {
            wins += 1;
            guards.push(g);
        }
    }
    assert_eq!(wins, 1, "exactly one concurrent caller must win: got {wins}");
}
