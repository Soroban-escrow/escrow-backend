//! Single-writer leader election using Postgres session-scoped advisory locks (#110, #111).
//!
//! The README says `serve` and `worker` can be scaled apart, but nothing stopped two `worker`
//! processes from both running the relayer loop (sequence collisions) or the indexer loop
//! (duplicate cursor saves, duplicate follow-up jobs). This module hands each singleton loop
//! a lock keyed by a well-known `i64`; `pg_try_advisory_lock` returns `false` to the second
//! caller, which then logs that it is standing by and retries at the loop's poll interval.
//!
//! The lock is held on a dedicated pooled connection; when the leader is dropped the
//! connection is returned to the pool (sqlx issues a reset that releases session-scoped
//! advisory locks). If the leader *process* dies the backend connection dies with it and
//! Postgres releases the lock automatically - the standby takes over on its next retry.

use sqlx::{PgPool, Postgres, pool::PoolConnection};

/// Keyspace for `pg_try_advisory_lock(id)`. Values are stable and chosen so a casual
/// `SELECT` in operations ("what is this process waiting on?") makes them identifiable.
///
/// Must not collide with transaction-scoped locks held elsewhere (e.g. the
/// `audit_log_chain` trigger's `hashtext('audit_log_chain')`): Postgres treats session
/// and transaction advisory locks as separate keyspaces, so they cannot interfere, but
/// keep the numeric range high enough that an accidental overlap stays unlikely.
pub const RELAYER_LOCK_ID: i64 = 0x7265_6c61_7900_0001; // "relay\0\0\0\x01"
pub const INDEXER_LOCK_ID: i64 = 0x696e_6465_7800_0001; // "index\0\0\0\x01"

/// Holds the advisory lock. Dropping it returns the connection to the pool (which
/// releases the lock) so another instance can acquire it on its next retry.
pub struct LeaderLock {
    conn: PoolConnection<Postgres>,
    lock_id: i64,
}

impl LeaderLock {
    pub fn lock_id(&self) -> i64 {
        self.lock_id
    }

    /// Quick keepalive. If the leader's backend connection has died (network blip, server
    /// restart) the lock has already been released on the Postgres side; the caller should
    /// drop this guard and go back to the acquire loop. Returns `Ok(())` while held.
    pub async fn ping(&mut self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&mut *self.conn).await.map(|_| ())
    }
}

/// Tries to acquire the lock. `Ok(Some(_))` means we are the leader; `Ok(None)` means a
/// peer holds it and the caller should back off and retry. `Err` is a database-level
/// failure (pool exhausted, query error) - the caller should log and retry.
pub async fn try_acquire(pool: &PgPool, lock_id: i64) -> Result<Option<LeaderLock>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    // `pg_try_advisory_lock` is non-blocking: it returns false if the lock is already held.
    let (got,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)").bind(lock_id).fetch_one(&mut *conn).await?;
    Ok(if got { Some(LeaderLock { conn, lock_id }) } else { None })
}
