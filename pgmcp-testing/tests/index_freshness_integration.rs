//! Real-Postgres integration oracle for the index-freshness work (v41
//! `last_verified_at` + v42 `index_failures` ledger).
//!
//! Proves the actual `pgmcp::db::queries` functions against a real schema —
//! catching SQL typos and validating the v41/v42 migrations + the v42 CHECK
//! constraint end-to-end. `require_test_db!` skips cleanly when no test DB is
//! configured, so this runs inside `verify.sh` without an `#[ignore]`.

use pgmcp::db::queries;
use pgmcp::embed::failure_kind::FailureKind;
use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project};
use pgmcp_testing::require_test_db;

/// `mark_files_verified` advances `last_verified_at` for the named set WITHOUT
/// touching `indexed_at` — this is the property that kills the false-staleness
/// signal (a git-touched-but-unchanged file is re-verified, but `indexed_at`
/// stays at the last *content* change). Also covers the empty-slice short
/// circuit and the single-row `mark_file_verified`.
#[tokio::test(flavor = "multi_thread")]
async fn mark_files_verified_advances_without_touching_indexed_at() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "freshness", "/ws/freshness").await;
    let path = "/ws/freshness/a.rs";
    seed_file(&pool, project, path, "a.rs").await;

    // Freshly seeded: last_verified_at is NULL (seed_file doesn't set it).
    let (indexed_before, verified_before): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT indexed_at, last_verified_at FROM indexed_files WHERE path = $1")
        .bind(path)
        .fetch_one(&pool)
        .await
        .expect("select before");
    assert!(verified_before.is_none(), "precondition: not yet verified");

    // Empty slice short-circuits to Ok(0) without a DB round-trip.
    assert_eq!(
        queries::mark_files_verified(&pool, &[])
            .await
            .expect("empty"),
        0
    );

    let rows = queries::mark_files_verified(&pool, &[path.to_string()])
        .await
        .expect("bulk verify");
    assert_eq!(rows, 1, "exactly the named row is marked");

    let (indexed_after, verified_after): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT indexed_at, last_verified_at FROM indexed_files WHERE path = $1")
        .bind(path)
        .fetch_one(&pool)
        .await
        .expect("select after");

    assert!(
        verified_after.is_some(),
        "last_verified_at advanced from NULL"
    );
    assert_eq!(
        indexed_before, indexed_after,
        "indexed_at must NOT change — only last_verified_at advances"
    );

    // Single-row variant works too (the live-event Level-2-skip path).
    let path2 = "/ws/freshness/b.rs";
    seed_file(&pool, project, path2, "b.rs").await;
    queries::mark_file_verified(&pool, path2)
        .await
        .expect("single verify");
    let v2: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_verified_at FROM indexed_files WHERE path = $1")
            .bind(path2)
            .fetch_one(&pool)
            .await
            .expect("select b");
    assert!(v2.is_some(), "single-row mark_file_verified set the column");
}

/// The `index_failures` ledger: `record_index_failure` UPSERTs (count climbs),
/// `get_bounded_failure_paths` honors the threshold, `clear_index_failure`
/// removes the row, and `failure_kind_counts` reports the breakdown.
#[tokio::test(flavor = "multi_thread")]
async fn index_failures_ledger_lifecycle() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let path = "/ws/freshness/corrupt.pdf";

    // First failure inserts count = 1; second bumps to 2.
    queries::record_index_failure(&pool, path, FailureKind::DocExtractFailed, "boom 1")
        .await
        .expect("record 1");
    queries::record_index_failure(&pool, path, FailureKind::DocExtractTimeout, "boom 2")
        .await
        .expect("record 2");

    let (count, kind): (i32, String) =
        sqlx::query_as("SELECT failure_count, failure_kind FROM index_failures WHERE path = $1")
            .bind(path)
            .fetch_one(&pool)
            .await
            .expect("select failure");
    assert_eq!(count, 2, "UPSERT incremented failure_count");
    assert_eq!(
        kind, "doc_extract_timeout",
        "kind reflects the latest failure"
    );

    // Bounded at threshold 2 (present), not at 3 (absent).
    let at_2 = queries::get_bounded_failure_paths(&pool, 2)
        .await
        .expect("bounded 2");
    assert!(
        at_2.iter().any(|f| f.path == path),
        "count 2 >= threshold 2 → bounded"
    );
    let at_3 = queries::get_bounded_failure_paths(&pool, 3)
        .await
        .expect("bounded 3");
    assert!(
        !at_3.iter().any(|f| f.path == path),
        "count 2 < threshold 3 → not bounded"
    );

    // Breakdown surfaces the kind.
    let counts = queries::failure_kind_counts(&pool).await.expect("counts");
    assert!(
        counts
            .iter()
            .any(|(k, c)| k == "doc_extract_timeout" && *c == 1),
        "failure_kind_counts reports the ledgered kind, got {counts:?}"
    );

    // Clear removes it.
    queries::clear_index_failure(&pool, path)
        .await
        .expect("clear");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM index_failures WHERE path = $1")
        .bind(path)
        .fetch_one(&pool)
        .await
        .expect("count after clear");
    assert_eq!(remaining, 0, "clear_index_failure deleted the row");
}

/// The v42 closed-vocab CHECK rejects a `failure_kind` outside `FailureKind`.
#[tokio::test(flavor = "multi_thread")]
async fn index_failures_check_rejects_unknown_kind() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let res = sqlx::query("INSERT INTO index_failures (path, failure_kind) VALUES ($1, $2)")
        .bind("/ws/freshness/x")
        .bind("not_a_real_kind")
        .execute(&pool)
        .await;
    assert!(
        res.is_err(),
        "the index_failures_kind_check CHECK must reject an unknown failure_kind"
    );
}
