//! Phase 1 memory-server integration tests.
//!
//! Covers the BGE-M3 migration plumbing from
//! `docs/memory-server/02-phases.md` Phase 1:
//!
//! - Schema: `embedding_v2 vector(1024)` + `embedding_signature TEXT`
//!   exist on `file_chunks` and `session_prompts`; HNSW indices built.
//! - Operator helpers: `migration_complete`, `promote_to_bge_m3`,
//!   `active_embedding_signature` behave as documented.
//! - Cutover dispatch: `recall_prompts_semantic` selects the correct
//!   column based on the query embedding dimension (384 → legacy,
//!   1024 → BGE-M3).
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.
//!
//! The actual BGE-M3 model inference test is `#[ignore]`-gated because
//! it triggers a ~1.2 GB HuggingFace download on a cold cache. Run with
//! `cargo test --test memory_phase1 -- --ignored` once the cache is
//! warm to validate end-to-end inference.

use pgmcp::cron::embedding_migration::{
    active_embedding_signature, migration_complete, promote_to_bge_m3,
};
use pgmcp::db::queries::recall_prompts_semantic;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn embedding_v2_column_exists_and_accepts_1024d_vector() {
    let db = require_test_db!();
    let pool = db.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/phase1-v2-col")
    .bind("phase1-v2-col")
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 10, 'fn f() {}', 1, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind("/ws/phase1-v2-col/a.rs")
    .bind("a.rs")
    .fetch_one(pool)
    .await
    .expect("indexed_file");
    let v: Vec<f32> = (0..1024).map(|i| if i == 17 { 1.0 } else { 0.0 }).collect();
    let vector = pgvector::Vector::from(v);

    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature)
         VALUES ($1, 0, 'content', 1, 1, $2, $3)",
    )
    .bind(file_id)
    .bind(&vector)
    .bind("bge-m3-v1")
    .execute(pool)
    .await
    .expect("insert chunk with 1024d embedding_v2");

    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM file_chunks WHERE file_id = $1 AND embedding_v2 IS NOT NULL",
    )
    .bind(file_id)
    .fetch_one(pool)
    .await
    .expect("count");
    assert_eq!(count, 1, "the 1024d row should be present");
}

#[tokio::test(flavor = "multi_thread")]
async fn active_embedding_signature_defaults_to_minilm() {
    let db = require_test_db!();
    let pool = db.pool();
    let sig = active_embedding_signature(pool)
        .await
        .expect("active_embedding_signature");
    assert_eq!(
        sig, "minilm-l6-v2",
        "pre-cutover signature must remain legacy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn promote_to_bge_m3_refuses_when_backlog_present_and_succeeds_with_force() {
    let db = require_test_db!();
    let pool = db.pool();

    // Insert a row that lacks embedding_v2 — simulates backlog.
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/backlog-test")
    .bind("backlog-test")
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 10, 'fn f() {}', 1, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind("/ws/backlog-test/x.rs")
    .bind("x.rs")
    .fetch_one(pool)
    .await
    .expect("indexed_file");
    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line)
         VALUES ($1, 0, 'has no embedding_v2', 1, 1)",
    )
    .bind(file_id)
    .execute(pool)
    .await
    .expect("backlog chunk insert");

    // migration_complete must report false now.
    let complete = migration_complete(pool)
        .await
        .expect("migration_complete check");
    assert!(
        !complete,
        "presence of NULL embedding_v2 row must keep migration_complete=false"
    );

    // Non-forced promote refuses.
    let err = promote_to_bge_m3(pool, false).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("migration incomplete"),
        "expected refusal text; got: {msg}"
    );

    // Forced promote succeeds and flips the signature.
    promote_to_bge_m3(pool, true)
        .await
        .expect("forced promote should succeed");
    let sig = active_embedding_signature(pool)
        .await
        .expect("active_embedding_signature");
    assert_eq!(sig, "bge-m3-v1");

    // Reset for other tests in this transaction... actually require_test_db
    // gives a fresh transaction per test so this is isolated already.
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_dispatch_picks_correct_column_by_query_dim() {
    let db = require_test_db!();
    let pool = db.pool();

    // Seed a session + two prompts: one with a 384d embedding (legacy),
    // one with a 1024d embedding (v2). Each lives on its own session so
    // the test can verify column-selection without cross-talk.
    use uuid::Uuid;
    let sess_legacy = Uuid::new_v4();
    let sess_v2 = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, sess_legacy, "/ws/recall-legacy", None)
        .await
        .expect("session_legacy");
    pgmcp::sessions::upsert_session(pool, sess_v2, "/ws/recall-v2", None)
        .await
        .expect("session_v2");

    let v384: Vec<f32> = (0..384).map(|i| if i == 5 { 1.0 } else { 0.0 }).collect();
    let v1024: Vec<f32> = (0..1024).map(|i| if i == 9 { 1.0 } else { 0.0 }).collect();

    let pgv_384 = pgvector::Vector::from(v384.clone());
    let pgv_1024 = pgvector::Vector::from(v1024.clone());

    sqlx::query(
        "INSERT INTO session_prompts (session_id, prompt_text, prompt_sha256, embedding, embedding_signature)
         VALUES ($1, 'legacy prompt', $2, $3, 'minilm-l6-v2')",
    )
    .bind(sess_legacy)
    .bind(pgmcp::sessions::prompt_sha256("legacy prompt"))
    .bind(&pgv_384)
    .execute(pool)
    .await
    .expect("legacy prompt row");

    sqlx::query(
        "INSERT INTO session_prompts (session_id, prompt_text, prompt_sha256, embedding_v2, embedding_signature)
         VALUES ($1, 'v2 prompt', $2, $3, 'bge-m3-v1')",
    )
    .bind(sess_v2)
    .bind(pgmcp::sessions::prompt_sha256("v2 prompt"))
    .bind(&pgv_1024)
    .execute(pool)
    .await
    .expect("v2 prompt row");

    // 384d query → reads `embedding` column → finds the legacy prompt only.
    let r_legacy = recall_prompts_semantic(pool, &v384, None, None, 5, 64)
        .await
        .expect("legacy recall");
    assert!(
        r_legacy.iter().any(|r| r.prompt_text == "legacy prompt"),
        "legacy column reads should surface 'legacy prompt'"
    );
    assert!(
        r_legacy.iter().all(|r| r.prompt_text != "v2 prompt"),
        "legacy column reads must NOT surface the v2-only row"
    );

    // 1024d query → reads `embedding_v2` column → finds the v2 prompt only.
    let r_v2 = recall_prompts_semantic(pool, &v1024, None, None, 5, 64)
        .await
        .expect("v2 recall");
    assert!(
        r_v2.iter().any(|r| r.prompt_text == "v2 prompt"),
        "v2 column reads should surface 'v2 prompt'"
    );
    assert!(
        r_v2.iter().all(|r| r.prompt_text != "legacy prompt"),
        "v2 column reads must NOT surface the legacy-only row"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_rejects_unsupported_query_dim() {
    let db = require_test_db!();
    let pool = db.pool();
    let v_bad: Vec<f32> = vec![0.0; 768]; // neither 384 nor 1024
    let err = recall_prompts_semantic(pool, &v_bad, None, None, 5, 64)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unsupported query-embedding dim"),
        "expected dim-rejection message; got: {msg}"
    );
}

// ============================================================================
// BGE-M3 model inference smoke test — heavy, opt-in via --ignored
// ============================================================================

/// End-to-end BGE-M3 embed smoke. Downloads ~1.2 GB on a cold HF cache.
/// Run with: `cargo test --test memory_phase1 -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads BGE-M3 weights (~1.2 GB) and runs candle inference; opt-in"]
async fn bge_m3_embedder_produces_1024d_l2_normalized_vectors() {
    let mut cfg = pgmcp::config::EmbeddingsConfig::default();
    cfg.model = "bge-m3".into();
    cfg.dimensions = 1024;
    cfg.use_gpu = std::env::var("PGMCP_TEST_USE_GPU").ok().as_deref() == Some("1");

    let embedder = pgmcp::embed::model::Embedder::new(&cfg).expect("Embedder::new for bge-m3");
    let texts = ["hello world", "search-engine optimization"];
    let vectors = embedder.embed(&texts).expect("embed");
    assert_eq!(vectors.len(), 2, "two inputs → two vectors");
    for v in &vectors {
        assert_eq!(v.len(), 1024, "BGE-M3 output must be 1024d");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "vector must be L2-normalized; got |v| = {norm}"
        );
    }
}
