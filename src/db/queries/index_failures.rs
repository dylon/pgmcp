//! Queries over the `index_failures` ledger (v42): the content-intrinsic
//! indexing-failure record that powers the scanner's bounded-retry gate and the
//! `index_stats` failure breakdown. See `src/embed/failure_kind.rs` for the
//! closed `FailureKind` vocabulary and `src/db/migrations/v42_index_failures.rs`
//! for the schema. Rows are cleared on a successful (re)index inside
//! `replace_indexed_file` / `insert_duplicate_file` / `update_file_path_in_place`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::embed::failure_kind::FailureKind;

/// One bounded-failure row, projected for the scanner's Level-1 gate.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IndexFailureMeta {
    pub path: String,
    pub last_failed_at: DateTime<Utc>,
}

/// Record (or bump) a content-intrinsic indexing failure for `path`. Idempotent
/// UPSERT: first failure inserts `failure_count = 1`; subsequent failures
/// increment the count and advance `last_failed_at`. Best-effort at the call
/// site — if the DB is unreachable the failure simply isn't ledgered (the
/// reconcile pass will re-attempt the file anyway).
pub async fn record_index_failure(
    pool: &PgPool,
    path: &str,
    kind: FailureKind,
    last_error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO index_failures (path, failure_kind, last_error)
         VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET
            failure_count = index_failures.failure_count + 1,
            last_failed_at = NOW(),
            failure_kind = EXCLUDED.failure_kind,
            last_error = EXCLUDED.last_error",
    )
    .bind(path)
    .bind(kind.as_str())
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Explicitly clear a file's failure-ledger row (e.g. when it is removed).
/// The common recovery path clears in-transaction inside the success writers;
/// this is the standalone form for callers without one.
pub async fn clear_index_failure(pool: &PgPool, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM index_failures WHERE path = $1")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

/// Paths whose failure_count has reached `min_failures` — the bounded set the
/// scanner skips re-submitting while their mtime has not advanced past
/// `last_failed_at`. Every ledgered kind is content-intrinsic (bounded), so no
/// kind filter is needed. Loaded once per scan, mirroring `get_all_file_metadata`.
pub async fn get_bounded_failure_paths(
    pool: &PgPool,
    min_failures: i32,
) -> Result<Vec<IndexFailureMeta>, sqlx::Error> {
    sqlx::query_as::<_, IndexFailureMeta>(
        "SELECT path, last_failed_at FROM index_failures WHERE failure_count >= $1",
    )
    .bind(min_failures)
    .fetch_all(pool)
    .await
}

/// `(failure_kind, count)` breakdown for `index_stats`, most frequent first.
pub async fn failure_kind_counts(pool: &PgPool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT failure_kind, COUNT(*)::bigint
         FROM index_failures
         GROUP BY failure_kind
         ORDER BY COUNT(*) DESC, failure_kind",
    )
    .fetch_all(pool)
    .await
}
