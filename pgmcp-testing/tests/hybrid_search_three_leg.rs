//! P13.2 — third RRF leg activation test.
//!
//! Trains a per-project HybridLM, then calls `tool_hybrid_search`
//! with `wfst_lm_weight = 1.0` and asserts:
//!   1. `legs_fused == 3` in the response (the third leg activated).
//!   2. The response includes a non-null `wfst_rewritten_query`.
//!
//! Together these prove that:
//!   - The training cron produces a loadable model file at the
//!     canonical path.
//!   - `tool_hybrid_search` discovers the model from
//!     `<data_dir>/hybrid_lm/<slug>-p<project_id>/model.bin`.
//!   - The lattice + LM rescoring produces a non-identity rewrite
//!     for at least one query.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::HybridSearchParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_hybrid_search::tool_hybrid_search;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

async fn seed_project_and_corpus(pool: &sqlx::PgPool, project_name: &str) -> i32 {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind(format!("/ws/{project_name}"))
    .bind(format!("/ws/{project_name}/p"))
    .bind(project_name)
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind(format!("/ws/{project_name}/p/src/lib.rs"))
    .bind("src/lib.rs")
    .bind(2048_i64)
    .bind("fn receive_request() {}\nfn process_response() {}")
    .bind(9999_i64)
    .bind(2_i32)
    .fetch_one(pool)
    .await
    .expect("file");

    // Chunk content drives both n-gram training and vocabulary
    // sampling. We seed identifiers ("receive", "process") so the
    // candidate generator has something close to a misspelled query
    // to lock on to.
    let backend = DeterministicEmbeddingBackend::new(1024);
    let chunks = [
        "receive request validate handler dispatch reply",
        "process response collect emit retry log",
        "receive socket parse decode encode emit transmit",
        "process event handler queue scheduler",
        "receive packet decode encode transmit",
        "process payload validate sign encrypt decrypt",
    ];
    for (i, content) in chunks.iter().enumerate() {
        let emb = backend.embed_one(content).await.expect("embed");
        let v = pgvector::Vector::from(emb);
        sqlx::query(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
             VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')"
        )
        .bind(file_id)
        .bind(i as i32)
        .bind(*content)
        .bind((i as i32) + 1)
        .bind((i as i32) + 1)
        .bind(v)
        .execute(pool)
        .await
        .expect("chunk");
    }

    // Symbol vocabulary the lattice candidate generator pulls from.
    for sym in ["receive_request", "process_response", "validate_request"] {
        sqlx::query(
            "INSERT INTO file_symbols (file_id, name, kind, visibility, start_line, end_line) \
             VALUES ($1, $2, 'function', 'public', 1, 1)
             ON CONFLICT DO NOTHING",
        )
        .bind(file_id)
        .bind(sym)
        .execute(pool)
        .await
        .expect("symbol");
    }
    project_id
}

#[tokio::test(flavor = "multi_thread")]
async fn third_leg_activates_with_trained_lm_and_misspelled_query() {
    let testdb = require_test_db!();
    let project_name = "three_leg_test";
    let project_id = seed_project_and_corpus(testdb.pool(), project_name).await;

    // Train the HybridLM into a tempdir.
    let tmp = tempfile::tempdir().expect("tempdir");
    let pool_arc = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    pgmcp::cron::ngram_lm_train::run_or_log(
        Arc::clone(&pool_arc),
        Arc::clone(&stats),
        tmp.path().to_path_buf(),
    )
    .await;
    let model_path =
        pgmcp::cron::ngram_lm_train::model_path_for_project(tmp.path(), project_id, project_name);
    assert!(
        model_path.exists(),
        "training prerequisite failed: model at {} missing",
        model_path.display()
    );

    // Point the daemon config's fuzzy.data_dir at our tempdir so the
    // tool finds the trained model.
    let mut cfg = Config::default();
    cfg.fuzzy.data_dir = tmp.path().to_path_buf();
    let config = Arc::new(ArcSwap::from_pointee(cfg));

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        Arc::clone(&stats),
        config,
        Arc::new(LogBroadcaster::new()),
        Arc::new(TaskStore::new()),
        DaemonLifecycle::new(),
    );

    let params = HybridSearchParams {
        // "recieve" is one transposition away from "receive_request"
        // (distance ≤ 2 with the Damerau-Levenshtein transducer).
        query: "recieve".to_string(),
        project: Some(project_name.to_string()),
        language: None,
        limit: Some(10),
        bm25_weight: Some(0.5),
        semantic_weight: Some(0.5),
        dedupe_worktrees: Some(false),
        wfst_lm_weight: Some(1.0),
        max_query_edit_distance: Some(2),
        return_type_tags: None,
        effects: None,
        scope_kind: None,
    };
    let result = tool_hybrid_search(&ctx, params)
        .await
        .expect("hybrid_search");
    let json_text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    let val: serde_json::Value = serde_json::from_str(&json_text).expect("parse");

    let legs = val
        .get("legs_fused")
        .and_then(|v| v.as_u64())
        .expect("legs_fused field");
    assert_eq!(
        legs, 3,
        "third leg must activate when LM model exists + rewrite changes; response = {val:#}"
    );

    let rewritten = val.get("wfst_rewritten_query");
    assert!(
        rewritten.map(|v| !v.is_null()).unwrap_or(false),
        "wfst_rewritten_query must be set in response when third leg fired"
    );
}
