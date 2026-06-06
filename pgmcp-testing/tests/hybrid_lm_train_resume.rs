//! P13.2 — ngram-lm-train resume-via-rerun smoke test.
//!
//! The training cron's resume behavior is to write `<path>.tmp` and
//! atomically rename to `<path>` on success. A partial write never
//! produces a corrupted model. This test exercises the property by:
//!   1. Running the training cron once (writes the model).
//!   2. Truncating the model file to 0 bytes (simulates a crash
//!      between truncate and rename).
//!   3. Running the cron a second time — the model file must be
//!      rebuilt and load cleanly.
//!
//! Catches regression where the cron skips re-training when the file
//! already exists (which would leave the broken 0-byte file in place).

use std::sync::Arc;

use pgmcp::stats::tracker::StatsTracker;
use pgmcp::wfst::hybrid_lm::PgmcpHybridLm;
use pgmcp_testing::require_test_db;

async fn seed(pool: &sqlx::PgPool) -> i32 {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind("/ws/lm_resume")
    .bind("/ws/lm_resume/p")
    .bind("lm_resume_test")
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'text', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind("/ws/lm_resume/p/text.txt")
    .bind("text.txt")
    .bind(1024_i64)
    .bind("seed")
    .bind(456_i64)
    .bind(10_i32)
    .fetch_one(pool)
    .await
    .expect("file");
    for i in 0..12 {
        let content = format!(
            "this is sample sentence {} with enough vocabulary tokens to train a small model",
            i
        );
        sqlx::query(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(file_id)
        .bind(i)
        .bind(content)
        .bind(i + 1)
        .bind(i + 1)
        .execute(pool)
        .await
        .expect("chunk");
    }
    project_id
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_rebuilds_model_after_corruption() {
    let testdb = require_test_db!();
    let project_id = seed(testdb.pool()).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let pool = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());

    pgmcp::cron::ngram_lm_train::run_or_log(
        Arc::clone(&pool),
        Arc::clone(&stats),
        tmp.path().to_path_buf(),
    )
    .await;

    let model_path = pgmcp::cron::ngram_lm_train::model_path_for_project(
        tmp.path(),
        project_id,
        "lm_resume_test",
    );
    assert!(model_path.exists(), "first run must persist model");
    let first_open = PgmcpHybridLm::open(&model_path);
    assert!(first_open.is_ok(), "first model loads cleanly");

    // Simulate corruption (truncate to 0 bytes).
    std::fs::write(&model_path, b"").expect("truncate");
    assert!(
        PgmcpHybridLm::open(&model_path).is_err(),
        "0-byte file must not load"
    );

    // Re-run; the cron must rebuild from scratch.
    pgmcp::cron::ngram_lm_train::run_or_log(
        Arc::clone(&pool),
        Arc::clone(&stats),
        tmp.path().to_path_buf(),
    )
    .await;
    assert!(
        std::fs::metadata(&model_path).expect("metadata").len() > 0,
        "rerun must rewrite model (file must be non-empty)"
    );
    assert!(
        PgmcpHybridLm::open(&model_path).is_ok(),
        "rebuilt model must load cleanly"
    );
}
