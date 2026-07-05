#![forbid(unsafe_code)]
//! Postgres access: schema migrations, typed rows and small, explicit repositories.
//!
//! Repository functions take any sqlx executor, so the same function runs against the pool or inside a
//! transaction (`&mut *tx`). Queries are runtime-checked (no build-time database needed) and covered by
//! tests that run against a real Postgres.

pub mod audit;
pub mod bounties;
pub mod disputes;
pub mod github;
pub mod intents;
pub mod jobs;
pub mod models;
pub mod payouts;
pub mod prs;
pub mod reviews;
pub mod serde_helpers;
pub mod sessions;
pub mod users;
pub mod webhooks;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

pub use models::*;
pub use sqlx::Error as DbError;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A connection pool with the schema applied.
#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
}

impl Db {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, DbError> {
        let pool = PgPoolOptions::new().max_connections(max_connections).acquire_timeout(Duration::from_secs(10)).connect(url).await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        MIGRATOR.run(&self.pool).await
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map(|_| ())
    }
}

/// Unique-constraint violation (Postgres 23505): the caller lost a race and should treat it as "already exists".
pub fn is_unique_violation(e: &DbError) -> bool {
    e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "23505")
}

/// A fresh opaque row id (24 lowercase hex characters).
pub fn new_id() -> String {
    escrow_core::security::random_hex::<12>()
}
