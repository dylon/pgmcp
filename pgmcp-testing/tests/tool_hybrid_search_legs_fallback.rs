//! P13.2 — hybrid_search legs-fused fallback test.
//!
//! When `wfst_lm_weight = 0.0` OR no per-project HybridLM model file
//! exists, the third RRF leg must not activate and the response must
//! report `legs_fused = 2` (the legacy 2-leg behavior). Catches
//! regression where the third leg fires inappropriately and changes
//! ranking for users who didn't opt in.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::HybridSearchParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_hybrid_search::tool_hybrid_search;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

fn build_ctx(cfg: Config, db: Arc<dyn DbClient>) -> SystemContext {
    let config = Arc::new(ArcSwap::from_pointee(cfg));
    let stats = Arc::new(StatsTracker::new());
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = DaemonLifecycle::new();
    SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    )
}

async fn seed_minimal(pool: &sqlx::PgPool) -> i32 {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind("/ws/test")
    .bind("/ws/test/legs_fallback")
    .bind("legs_fallback")
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind("/ws/test/legs_fallback/src/lib.rs")
    .bind("src/lib.rs")
    .bind(64_i64)
    .bind("fn hello_world() {}\nfn process_request() {}")
    .bind(7777_i64)
    .bind(2_i32)
    .fetch_one(pool)
    .await
    .expect("file");

    use pgmcp::embed::EmbeddingBackend;
    let backend = DeterministicEmbeddingBackend::new(1024);
    let chunks = ["fn hello_world() {}", "fn process_request() {}"];
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
        .bind(1_i32)
        .bind(1_i32)
        .bind(v)
        .execute(pool)
        .await
        .expect("chunk");
    }
    project_id
}

#[tokio::test(flavor = "multi_thread")]
async fn legs_fused_two_when_wfst_lm_weight_is_zero() {
    let testdb = require_test_db!();
    let _ = seed_minimal(testdb.pool()).await;

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let ctx = build_ctx(Config::default(), db);

    let params = HybridSearchParams {
        query: "hello".to_string(),
        project: Some("legs_fallback".to_string()),
        language: None,
        limit: Some(10),
        bm25_weight: Some(0.5),
        semantic_weight: Some(0.5),
        dedupe_worktrees: Some(false),
        // Explicit opt-out → third leg must NOT fire.
        wfst_lm_weight: Some(0.0),
        max_query_edit_distance: Some(2),
        return_type_tags: None,
        effects: None,
        scope_kind: None,
    };
    let result = tool_hybrid_search(&ctx, params).await.expect("call");
    let json = extract_text(&result);
    let val: serde_json::Value = serde_json::from_str(&json).expect("json");
    let legs = val
        .get("legs_fused")
        .and_then(|v| v.as_u64())
        .expect("legs_fused field present");
    assert_eq!(legs, 2, "wfst_lm_weight=0 must keep 2-leg behavior");
    assert!(
        val.get("wfst_rewritten_query")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "no rewrite expected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn legs_fused_two_when_no_lm_model_file_present() {
    let testdb = require_test_db!();
    let _ = seed_minimal(testdb.pool()).await;

    // Point fuzzy.data_dir at an empty tempdir → no LM file exists →
    // the path-existence check inside try_third_leg returns None and
    // we fall back to 2 legs.
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::default();
    cfg.fuzzy.data_dir = tmp.path().to_path_buf();
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let ctx = build_ctx(cfg, db);

    let params = HybridSearchParams {
        query: "hello".to_string(),
        project: Some("legs_fallback".to_string()),
        language: None,
        limit: Some(10),
        bm25_weight: Some(0.5),
        semantic_weight: Some(0.5),
        dedupe_worktrees: Some(false),
        // Caller opts IN, but no model file exists.
        wfst_lm_weight: Some(1.0),
        max_query_edit_distance: Some(2),
        return_type_tags: None,
        effects: None,
        scope_kind: None,
    };
    let result = tool_hybrid_search(&ctx, params).await.expect("call");
    let json = extract_text(&result);
    let val: serde_json::Value = serde_json::from_str(&json).expect("json");
    let legs = val
        .get("legs_fused")
        .and_then(|v| v.as_u64())
        .expect("legs_fused field present");
    assert_eq!(
        legs, 2,
        "missing per-project HybridLM model must keep 2-leg"
    );
}

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    for content in &result.content {
        if let Some(text) = content.as_text() {
            return text.text.clone();
        }
    }
    panic!("no text content in CallToolResult");
}
