//! Followup 1 verification — `run_migrations` must be idempotent under
//! repeated invocation and must record its baseline version exactly once.

use pgmcp::config::VectorConfig;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn run_migrations_records_initial_schema_version_once() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let vector_cfg = VectorConfig::default();

    // The harness already ran migrations once when seeding the template
    // database — confirm the baseline version is recorded.
    let initial: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pgmcp_schema_versions WHERE version = 1 AND name = 'initial_schema'",
    )
    .fetch_one(&pool)
    .await
    .expect("schema versions query must succeed");
    assert_eq!(
        initial, 1,
        "initial schema version must be recorded after the harness migration"
    );

    // Running migrations again must be a no-op for the version rows —
    // ON CONFLICT DO NOTHING + the version_applied short-circuit ensure
    // we don't double-insert and `applied_at` is unchanged.
    let applied_at_before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT applied_at FROM pgmcp_schema_versions WHERE version = 1")
            .fetch_one(&pool)
            .await
            .expect("applied_at lookup");
    let count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pgmcp_schema_versions")
        .fetch_one(&pool)
        .await
        .expect("count query");

    pgmcp::db::migrations::run_migrations(&pool, &vector_cfg)
        .await
        .expect("second migration run must succeed (idempotent)");

    let applied_at_after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT applied_at FROM pgmcp_schema_versions WHERE version = 1")
            .fetch_one(&pool)
            .await
            .expect("applied_at lookup");
    assert_eq!(
        applied_at_before, applied_at_after,
        "applied_at must not change on a no-op rerun"
    );

    // The version set is stable across an idempotent rerun: every numbered
    // step (version 1 plus each vN submodule) is gated by `version_applied`,
    // so a second run records nothing new. Assert the count is *unchanged*
    // rather than hardcoding a single expected value — the old `== 1` was
    // only true before v2..vN existed and would silently rot as steps are
    // added (it survived only because this test self-skips without a
    // CREATEDB-privileged test role).
    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pgmcp_schema_versions")
        .fetch_one(&pool)
        .await
        .expect("count query");
    assert_eq!(
        count_before, count_after,
        "no new version rows may be inserted on a no-op rerun"
    );
    assert!(
        count_before >= 1,
        "at least the initial_schema baseline version must be recorded"
    );
}

#[tokio::test]
async fn work_item_code_anchor_chunk_fk_index_exists() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM pg_indexes
             WHERE schemaname = 'public'
               AND tablename = 'work_item_code_anchor'
               AND indexname = 'idx_wi_anchor_chunk'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index existence query");

    assert!(
        exists,
        "work_item_code_anchor.chunk_id must be indexed for file_chunks delete cascades"
    );
}
