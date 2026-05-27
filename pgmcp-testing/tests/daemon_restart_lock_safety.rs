//! Regression: a daemon restart that lands while a heavy analytic query holds
//! locks must not abort startup with `canceling statement due to lock timeout`.
//!
//! On 2026-05-27 a restart during the 6-hour `semantic-edges` cron failed to
//! boot: the previous instance's runtime was dropped mid-query, the PostgreSQL
//! backend was orphaned (held `ACCESS SHARE` on `indexed_files`), and the new
//! instance's first `ALTER TABLE indexed_files` (the unguarded
//! `content_hash DROP NOT NULL`) hit `lock_timeout` and `?`-propagated out of
//! startup. These tests cover the three fixes:
//!
//!  * H2 — [`run_migrations_with_lock_retry`] retries on SQLSTATE 55P03 instead
//!    of aborting, so startup rides out transient contention.
//!  * H3 — the `content_hash DROP NOT NULL` is guarded on current nullability,
//!    so the common (already-nullable) boot issues no ACCESS-EXCLUSIVE ALTER and
//!    re-running migrations stays a no-op.
//!  * H4 — [`terminate_heavy_backends`] reaps exactly the labeled heavy backends
//!    during graceful shutdown, freeing their locks at the source.

use pgmcp::config::VectorConfig;
use pgmcp::db::admin::terminate_heavy_backends;
use pgmcp::db::migrations::{run_migrations, run_migrations_with_lock_retry};
use pgmcp_testing::require_test_db;
use sqlx::{Connection, PgConnection, PgPool};
use std::time::Duration;

/// H3: forcing `content_hash` back to `NOT NULL` and re-running migrations must
/// (a) succeed, (b) leave the column nullable, and (c) stay a no-op on a second
/// run (the nullability guard skips the ALTER).
#[tokio::test]
async fn migrations_guard_content_hash_drop_not_null() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // The template DB is schema-only (empty), so SET NOT NULL succeeds.
    sqlx::query("ALTER TABLE indexed_files ALTER COLUMN content_hash SET NOT NULL")
        .execute(&pool)
        .await
        .expect("force content_hash NOT NULL");

    run_migrations(&pool, &VectorConfig::default())
        .await
        .expect("migrations must succeed after content_hash was forced NOT NULL");

    let is_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'indexed_files'
           AND column_name = 'content_hash'",
    )
    .fetch_one(&pool)
    .await
    .expect("query content_hash nullability");
    assert_eq!(
        is_nullable, "YES",
        "content_hash must be nullable after migrations"
    );

    // Idempotent: the guard now sees a nullable column and skips the ALTER.
    run_migrations(&pool, &VectorConfig::default())
        .await
        .expect("re-running migrations on an already-nullable column must stay Ok");
}

/// H4: the shutdown sweep terminates exactly the backends carrying the
/// `pgmcp:heavy:<job>` label and leaves unlabeled backends (and the caller)
/// untouched.
#[tokio::test]
async fn terminate_heavy_backends_targets_only_labeled() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let url = db.connection_url();

    // Heavy-labeled holder: a transaction stamped exactly like a real heavy cron
    // (`SET LOCAL application_name = 'pgmcp:heavy:<job>'`) holding a table lock.
    let mut heavy = PgConnection::connect(&url)
        .await
        .expect("connect heavy holder");
    sqlx::query("BEGIN")
        .execute(&mut heavy)
        .await
        .expect("begin heavy");
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:test'")
        .execute(&mut heavy)
        .await
        .expect("label heavy txn");
    sqlx::query("LOCK TABLE indexed_files IN ACCESS SHARE MODE")
        .execute(&mut heavy)
        .await
        .expect("heavy holder takes lock");

    // Control holder: same shape, but the default (unlabeled) application_name.
    let mut control = PgConnection::connect(&url)
        .await
        .expect("connect control holder");
    sqlx::query("BEGIN")
        .execute(&mut control)
        .await
        .expect("begin control");
    sqlx::query("LOCK TABLE indexed_files IN ACCESS SHARE MODE")
        .execute(&mut control)
        .await
        .expect("control holder takes lock");

    // Let pg_stat_activity reflect both sessions' application_name.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let terminated = terminate_heavy_backends(&pool)
        .await
        .expect("heavy-backend sweep");
    assert!(
        terminated >= 1,
        "the heavy-labeled backend must be terminated (got {terminated})"
    );

    // The heavy holder's connection was terminated: its next statement errors.
    let heavy_after = sqlx::query("SELECT 1").execute(&mut heavy).await;
    assert!(
        heavy_after.is_err(),
        "the heavy-labeled backend must be terminated"
    );

    // The control holder was not labeled, so the sweep left it alone.
    let control_after = sqlx::query("SELECT 1").execute(&mut control).await;
    assert!(
        control_after.is_ok(),
        "an unlabeled backend must NOT be terminated by the heavy sweep"
    );
}

/// H2: with a conflicting `ACCESS EXCLUSIVE` lock held, the first migration
/// attempt hits `lock_timeout` (55P03); the wrapper retries and succeeds once
/// the lock is released, instead of aborting startup.
#[tokio::test]
async fn migrations_retry_through_lock_contention() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let url = db.connection_url();

    // Make new sessions to this DB fail a lock wait after 1s so the first
    // migration attempt raises 55P03 rather than blocking indefinitely.
    sqlx::query(&format!(
        "ALTER DATABASE \"{}\" SET lock_timeout = '1s'",
        db.db_name()
    ))
    .execute(&pool)
    .await
    .expect("set per-database lock_timeout");

    // This pool's connections postdate the ALTER, so they inherit lock_timeout.
    let migration_pool = PgPool::connect(&url).await.expect("migration pool");

    // Hold ACCESS EXCLUSIVE on indexed_files so migration DDL touching it blocks.
    let mut holder = PgConnection::connect(&url)
        .await
        .expect("connect lock holder");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin holder");
    sqlx::query("LOCK TABLE indexed_files IN ACCESS EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("holder takes ACCESS EXCLUSIVE");

    // Run migrations concurrently; they must retry, not abort.
    let migrations = tokio::spawn(async move {
        run_migrations_with_lock_retry(&migration_pool, &VectorConfig::default()).await
    });

    // Release the lock after the first attempt times out (~1s) but before the
    // 5s retry backoff elapses, so the retry finds the table free.
    tokio::time::sleep(Duration::from_secs(2)).await;
    sqlx::query("ROLLBACK")
        .execute(&mut holder)
        .await
        .expect("release lock");
    drop(holder);

    let outcome = tokio::time::timeout(Duration::from_secs(60), migrations)
        .await
        .expect("migrations did not finish within 60s")
        .expect("migration task panicked");
    outcome.expect("run_migrations_with_lock_retry must succeed once contention clears");
}
