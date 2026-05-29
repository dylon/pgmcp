//! P14.5 — `tool_hybrid_search`'s third RRF leg consumes the
//! persistent symbol trie, NOT a per-call PG SELECT.
//!
//! The pre-P14.5 third leg ran `SELECT DISTINCT lower(fs.name)
//! FROM file_symbols ...` on every call and built a transient
//! `DynamicDawgChar` from the result. Post-P14.5 it opens the
//! per-project `FuzzyIndex<SymbolValue>` (lazy-warming it from PG
//! on first call if needed). This test proves the new behavior:
//!
//! - Pre-populate the symbol trie at `<data_dir>/fuzzy/symbols/<slug>/symbols.artrie`
//!   with `receive_handler`. Do NOT seed `file_symbols` in PG.
//! - Train the HybridLM in the same tempdir.
//! - Call `tool_hybrid_search` with the misspelled query `recieve`.
//! - Assert: `legs_fused == 3` and `wfst_rewritten_query` mentions
//!   the trie-only symbol. Proves the third leg consulted the
//!   trie (PG has no `file_symbols` row, so a PG-backed code path
//!   would have returned an empty vocabulary and skipped the leg).

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::cron::fuzzy_sync::{slugify, trie_path};
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::fuzzy::persistent_artrie::FuzzyIndex;
use pgmcp::fuzzy::values::SymbolValue;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::HybridSearchParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_hybrid_search::tool_hybrid_search;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

async fn seed_project_and_chunks(pool: &sqlx::PgPool, project_name: &str) -> i32 {
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
    .bind("fn receive_handler() {}")
    .bind(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0) ^ (project_name.len() as i64),
    )
    .bind(2_i32)
    .fetch_one(pool)
    .await
    .expect("file");

    // Chunks for HybridLM training (needs ≥50 tokens). Symbols NOT
    // seeded — only chunks. The third leg's lazy-warm would find
    // an empty file_symbols set for this project, so a regression
    // back to the PG path returns a zero-vocab response.
    let backend = DeterministicEmbeddingBackend::new(1024);
    let chunks = [
        "receive handler dispatch reply process emit",
        "receive socket parse decode encode transmit",
        "receive packet validate sign encrypt decrypt",
        "receive event scheduler queue worker",
        "receive payload write commit retry log",
        "receive request validate handler dispatch reply",
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

    project_id
}

#[tokio::test(flavor = "multi_thread")]
async fn third_leg_pulls_candidates_from_persistent_trie() {
    let testdb = require_test_db!();
    let project_name = "third_leg_trie_test";
    let _ = seed_project_and_chunks(testdb.pool(), project_name).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let pool_arc = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());

    // 1. Pre-populate the symbol trie with the authoritative symbol.
    //    PG's `file_symbols` is intentionally empty for this project.
    let symbols_path = trie_path(tmp.path(), "symbols", &slugify(project_name));
    let (sym_idx, _recovery) =
        FuzzyIndex::<SymbolValue>::open_or_create(&symbols_path).expect("symbol trie create");
    sym_idx
        .upsert(
            "receive_handler",
            SymbolValue {
                file_id: 999,
                kind: "function".to_string(),
                visibility: "public".to_string(),
                line: 1,
            },
        )
        .expect("trie upsert");
    drop(sym_idx);

    // 2. Train the per-project HybridLM (same tempdir).
    pgmcp::cron::ngram_lm_train::run_or_log(
        Arc::clone(&pool_arc),
        Arc::clone(&stats),
        tmp.path().to_path_buf(),
    )
    .await;
    let model_path = pgmcp::cron::ngram_lm_train::model_path_for(tmp.path(), project_name);
    assert!(
        model_path.exists(),
        "training prerequisite failed: model at {} missing",
        model_path.display()
    );

    // 3. Build a SystemContext that points fuzzy.data_dir at the
    //    same tempdir so the third leg finds both the trie and the
    //    LM.
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

    // 4. Misspelled query → third leg should rewrite via the trie.
    let params = HybridSearchParams {
        query: "recieve".to_string(),
        project: Some(project_name.to_string()),
        language: None,
        limit: Some(10),
        bm25_weight: Some(0.5),
        semantic_weight: Some(0.5),
        dedupe_worktrees: Some(false),
        wfst_lm_weight: Some(1.0),
        max_query_edit_distance: Some(3),
        return_type_tags: None,
        effects: None,
        scope_kind: None,
    };
    let result = tool_hybrid_search(&ctx, params).await.expect("call");
    let json_text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    let val: serde_json::Value = serde_json::from_str(&json_text).expect("json");

    let legs = val
        .get("legs_fused")
        .and_then(|v| v.as_u64())
        .expect("legs_fused field");
    assert_eq!(
        legs, 3,
        "third leg must fire (LM + trie both present); resp = {val:#}"
    );
    let rewritten = val
        .get("wfst_rewritten_query")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        rewritten.contains("receive_handler"),
        "rewrite must reference the trie-only symbol (proves the trie was consulted, \
         not PG — PG has no file_symbols rows for this project). got: {rewritten:?}\n\n{val:#}"
    );
}
