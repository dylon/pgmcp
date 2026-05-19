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

    // Running migrations again must be a no-op for the version row —
    // ON CONFLICT DO NOTHING + the version_applied short-circuit ensure
    // we don't double-insert and `applied_at` is unchanged.
    let applied_at_before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT applied_at FROM pgmcp_schema_versions WHERE version = 1")
            .fetch_one(&pool)
            .await
            .expect("applied_at lookup");

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

    let still_one: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pgmcp_schema_versions")
        .fetch_one(&pool)
        .await
        .expect("count query");
    assert_eq!(still_one, 1, "only one version row should exist");
}
