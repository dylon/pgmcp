//! Database connection pool management.
//!
//! The pool is configured to fail loud rather than silently degrade:
//!
//! - **Per-session timeouts** (`statement_timeout`, `lock_timeout`,
//!   `idle_in_transaction_session_timeout`) cap how long any single query
//!   or lock-wait can hold a connection. Long-running analytic queries
//!   raise their own ceiling via `SET LOCAL` inside a transaction.
//! - **Pool-level timeouts** (`idle_timeout`, `max_lifetime`) recycle
//!   connections through natural churn, so a Postgres restart surfaces
//!   as a quick reconnect rather than a wall of "broken pipe" errors.
//! - **`test_before_acquire`** issues `SELECT 1` on each checkout, trading
//!   one round-trip for the guarantee that the caller never receives a
//!   dead connection.

use std::time::Duration;

use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::config::DatabaseConfig;

/// Create a connection pool from the database configuration.
pub async fn create_pool(config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    let statement_timeout_ms = config.statement_timeout_ms;
    let idle_in_tx_timeout_ms = config.idle_in_transaction_timeout_ms;
    let lock_timeout_ms = config.lock_timeout_ms;

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(config.pool_idle_timeout_secs))
        .max_lifetime(Duration::from_secs(config.pool_max_lifetime_secs))
        .test_before_acquire(config.test_before_acquire)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                let sql = format!(
                    "SET statement_timeout = {statement_timeout_ms}; \
                     SET idle_in_transaction_session_timeout = {idle_in_tx_timeout_ms}; \
                     SET lock_timeout = {lock_timeout_ms};"
                );
                conn.execute(sql.as_str()).await?;
                Ok(())
            })
        })
        .connect(&config.connection_url())
        .await?;

    Ok(pool)
}

/// Health check — run a simple query to verify connectivity.
pub async fn health_check(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
