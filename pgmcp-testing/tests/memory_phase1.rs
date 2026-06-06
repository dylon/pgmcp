//! Phase 1 memory-server integration tests.
//!
//! Covers the BGE-M3/1024-only memory plumbing:
//!
//! - Schema: `embedding_v2 vector(1024)` + `embedding_signature TEXT`
//!   exist on `file_chunks` and `session_prompts`; HNSW indices built.
//! - Active signature: `read_active_signature` resolves to the only
//!   supported signature (`bge-m3-v1`) — there is no legacy default.
//! - Read path: `recall_prompts_semantic` always reads the 1024-d
//!   `embedding_v2` column and rejects any non-1024 query embedding.
//!
//! The legacy 384-d MiniLM dual-column migration window has been removed:
//! the former dual-dim dispatch and `promote_to_bge_m3` backlog-gating
//! tests are gone, rewritten here to assert the 1024-only invariants.
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.
//!
//! The actual BGE-M3 model inference test is `#[ignore]`-gated because
//! it triggers a ~1.2 GB HuggingFace download on a cold cache. Run with
//! `cargo test --test memory_phase1 -- --ignored` once the cache is
//! warm to validate end-to-end inference.

use pgmcp::db::queries::recall_prompts_semantic;
use pgmcp::embed::signature::read_active_signature;
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
async fn active_signature_resolves_to_bge_m3() {
    let db = require_test_db!();
    let pool = db.pool();
    // BGE-M3/1024-only: with no metadata row (or any value), the active
    // signature resolves to the single supported signature. There is no
    // legacy MiniLM default anymore.
    let sig = read_active_signature(pool)
        .await
        .expect("read_active_signature");
    assert_eq!(
        sig.as_str(),
        "bge-m3-v1",
        "the only supported signature must be bge-m3-v1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn active_signature_reads_back_stamped_metadata() {
    let db = require_test_db!();
    let pool = db.pool();
    // Stamping the canonical signature into pgmcp_metadata round-trips
    // through the resolver. (The migration-window `promote_to_bge_m3`
    // backlog-gating helper was removed alongside the legacy path; the
    // durable invariant is simply that the stamped value is read back.)
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('active_embedding_signature', 'bge-m3-v1')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(pool)
    .await
    .expect("stamp bge-m3-v1 signature");
    let sig = read_active_signature(pool)
        .await
        .expect("read_active_signature");
    assert_eq!(sig.as_str(), "bge-m3-v1");
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_reads_embedding_v2_for_1024d_query() {
    let db = require_test_db!();
    let pool = db.pool();

    // Seed one session + one prompt with a 1024d embedding in the
    // canonical `embedding_v2` column. A 1024d query must surface it.
    use uuid::Uuid;
    let sess_v2 = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, sess_v2, "/ws/recall-v2", None)
        .await
        .expect("session_v2");

    let v1024: Vec<f32> = (0..1024).map(|i| if i == 9 { 1.0 } else { 0.0 }).collect();
    let pgv_1024 = pgvector::Vector::from(v1024.clone());

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

    // 1024d query → reads `embedding_v2` column → finds the v2 prompt.
    let r_v2 = recall_prompts_semantic(pool, &v1024, None, None, 5, 64)
        .await
        .expect("v2 recall");
    assert!(
        r_v2.iter().any(|r| r.prompt_text == "v2 prompt"),
        "embedding_v2 reads should surface 'v2 prompt'"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_rejects_non_1024d_query_dim() {
    let db = require_test_db!();
    let pool = db.pool();
    // Both the former legacy MiniLM dim (384) and any other non-1024 dim
    // are rejected: BGE-M3/1024 is the only supported query shape.
    for bad_dim in [384usize, 768] {
        let v_bad: Vec<f32> = vec![0.0; bad_dim];
        let err = recall_prompts_semantic(pool, &v_bad, None, None, 5, 64)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("1024-dimension BGE-M3"),
            "expected 1024-only dim-rejection message for dim {bad_dim}; got: {msg}"
        );
    }
}

// ============================================================================
// BGE-M3 model inference smoke test — heavy, opt-in via --ignored
// ============================================================================

/// End-to-end BGE-M3 embed smoke. Downloads ~1.2 GB on a cold HF cache.
/// Run with: `cargo test --test memory_phase1 -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads BGE-M3 weights (~1.2 GB) and runs candle inference; opt-in"]
async fn bge_m3_embedder_produces_1024d_l2_normalized_vectors() {
    let cfg = pgmcp::config::EmbeddingsConfig {
        model: "bge-m3".into(),
        dimensions: 1024,
        use_gpu: std::env::var("PGMCP_TEST_USE_GPU").ok().as_deref() == Some("1"),
        ..Default::default()
    };

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
