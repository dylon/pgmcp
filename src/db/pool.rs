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
    let client_conn_check_ms = config.client_connection_check_interval_ms;

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(config.pool_idle_timeout_secs))
        .max_lifetime(Duration::from_secs(config.pool_max_lifetime_secs))
        .test_before_acquire(config.test_before_acquire)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                // Base per-session GUCs — valid on every supported PostgreSQL
                // version. `application_name = 'pgmcp'` labels our backends in
                // `pg_stat_activity`; heavy cron transactions override it with
                // `SET LOCAL application_name = 'pgmcp:heavy:<job>'` so the
                // graceful-shutdown sweep can target them (see src/db/admin.rs).
                //
                // `client_min_messages = warning` raises the floor below which the
                // server does not send messages to us. PostgreSQL emits a stream
                // of benign NOTICE-level messages that sqlx otherwise surfaces at
                // INFO and floods the logs — they are informational, not problems:
                //   • "word is too long to be indexed" — `to_tsvector` (the
                //     `file_chunks.content_tsv` GENERATED column + the GIN FTS
                //     indexes) skips any single lexeme over PostgreSQL's ~2 KB
                //     limit (minified lines, base64/hex blobs, long no-whitespace
                //     tokens). The row is still stored and embedded; only that
                //     over-long token is omitted from the full-text index, which
                //     is correct — such blobs are not meaningful FTS terms.
                //   • "relation/column ... already exists, skipping" — emitted by
                //     the idempotent `CREATE ... IF NOT EXISTS` / `ADD COLUMN IF
                //     NOT EXISTS` migrations on every startup.
                // Genuine WARNING/ERROR messages (e.g. pgvector's
                // "hnsw graph no longer fits into maintenance_work_mem") are at or
                // above WARNING and still surface. Suppressing at the source means
                // the server never sends them, so there is nothing for sqlx to log.
                let base = format!(
                    "SET application_name = 'pgmcp'; \
                     SET client_min_messages = warning; \
                     SET statement_timeout = {statement_timeout_ms}; \
                     SET idle_in_transaction_session_timeout = {idle_in_tx_timeout_ms}; \
                     SET lock_timeout = {lock_timeout_ms};"
                );
                conn.execute(base.as_str()).await?;

                // `client_connection_check_interval` was introduced in PostgreSQL
                // 14; it is an unknown GUC before that, and a failed `SET` would
                // fail the whole connection. Gate on the live server version so a
                // single config works across server versions.
                if client_conn_check_ms > 0 {
                    let server_version_num: i32 =
                        sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
                            .fetch_one(&mut *conn)
                            .await?;
                    if server_version_num >= 140_000 {
                        conn.execute(
                            format!(
                                "SET client_connection_check_interval = {client_conn_check_ms}"
                            )
                            .as_str(),
                        )
                        .await?;
                    }
                }
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
