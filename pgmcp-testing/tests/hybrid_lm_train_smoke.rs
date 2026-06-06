//! P13.2 — ngram-lm-train smoke test.
//!
//! Seeds a small corpus, runs the training cron, asserts the
//! bincode model file is created at the expected path AND that the
//! model round-trips via `PgmcpHybridLm::open`.

use std::sync::Arc;

use pgmcp::stats::tracker::StatsTracker;
use pgmcp::wfst::hybrid_lm::PgmcpHybridLm;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn cron_writes_and_reload_round_trips() {
    let testdb = require_test_db!();
    let pool = Arc::new(testdb.pool().clone());

    // Seed a project + a chunk corpus large enough that the
    // subword-embedding trainer is happy (≥50 tokens).
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind("/ws/lm_train_test")
    .bind("/ws/lm_train_test/proj")
    .bind("lm_train_test")
    .fetch_one(testdb.pool())
    .await
    .expect("project");

    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'text', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind("/ws/lm_train_test/proj/sample.txt")
    .bind("sample.txt")
    .bind(1024_i64)
    .bind("seed")
    .bind(123_i64)
    .bind(10_i32)
    .fetch_one(testdb.pool())
    .await
    .expect("file");

    // 12 chunks × 8 tokens = ~96 tokens, well above the 50-token
    // floor in train_project.
    for i in 0..12 {
        let content = format!(
            "the quick brown fox jumped over chunk {} today the cat sat on the mat",
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
        .execute(testdb.pool())
        .await
        .expect("chunk");
    }

    // Run the cron pointed at a tempdir so no global state leaks
    // between tests.
    let tmp = tempfile::tempdir().expect("tempdir");
    let stats = Arc::new(StatsTracker::new());
    pgmcp::cron::ngram_lm_train::run_or_log(
        Arc::clone(&pool),
        Arc::clone(&stats),
        tmp.path().to_path_buf(),
    )
    .await;

    assert!(
        stats
            .ngram_lm_train_runs
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "training cron must increment runs counter"
    );

    let model_path = pgmcp::cron::ngram_lm_train::model_path_for_project(
        tmp.path(),
        project_id,
        "lm_train_test",
    );
    assert!(
        model_path.exists(),
        "training cron must persist model to {}",
        model_path.display()
    );

    let lm = PgmcpHybridLm::open(&model_path).expect("model round-trips");
    // Score a token the corpus has seen many times. The score is a
    // log-probability; we only assert that it's finite (i.e. no
    // panic, no NaN) — the actual value depends on smoothing.
    let score = lm.score_continuation(&["the"], "quick");
    assert!(
        score.is_finite(),
        "score must be a finite log-prob, got {score}"
    );
}
