//! P14.3 — `tool_fuzzy_symbol_search` consults the persistent
//! `FuzzyIndex`, not PG, once the trie file exists.
//!
//! Two sub-tests:
//!
//! - `lazy_warm_from_empty`: fresh tempdir → no trie file. PG has
//!   `alpha_unique_function`. Tool call should lazy-warm from PG
//!   and return the symbol. Then assert the trie file now exists
//!   on disk — proves persistence (subsequent calls would skip the
//!   rebuild and serve from mmap).
//!
//! - `persistent_trie_beats_stale_pg`: pre-populate the trie with
//!   `trie_authoritative_func`. Seed PG with a DIFFERENT symbol
//!   (`pg_old_func`). Call the tool. Assert: response contains
//!   `trie_authoritative_func` and **not** `pg_old_func`. Proves
//!   that once the trie file exists, the tool reads only the trie.
//!
//! Together: lazy-warm works, and persistence is real (PG isn't
//! re-consulted on subsequent calls).

use std::sync::Arc;

use arc_swap::ArcSwap;
use libdictenstein::Dictionary;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::cron::fuzzy_sync::{slugify, trie_path};
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::fuzzy::persistent_artrie::FuzzyIndex;
use pgmcp::fuzzy::values::SymbolValue;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::FuzzySymbolSearchParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_fuzzy_symbol_search;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

async fn seed_project_with_symbols(pool: &sqlx::PgPool, project_name: &str, symbol_names: &[&str]) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind(format!("/ws/{project_name}"))
    .bind(format!("/ws/{project_name}/proj"))
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
    .bind(format!("/ws/{project_name}/proj/src/lib.rs"))
    .bind("src/lib.rs")
    .bind(1024_i64)
    .bind("seed")
    .bind(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0) ^ (project_name.len() as i64),
    )
    .bind(10_i32)
    .fetch_one(pool)
    .await
    .expect("file");
    for name in symbol_names {
        sqlx::query(
            "INSERT INTO file_symbols (file_id, name, kind, visibility, start_line, end_line) \
             VALUES ($1, $2, 'function', 'public', 1, 1)
             ON CONFLICT DO NOTHING",
        )
        .bind(file_id)
        .bind(*name)
        .execute(pool)
        .await
        .expect("symbol");
    }
}

fn build_ctx_with_data_dir(db: Arc<dyn DbClient>, data_dir: std::path::PathBuf) -> SystemContext {
    let mut cfg = Config::default();
    cfg.fuzzy.data_dir = data_dir;
    let config = Arc::new(ArcSwap::from_pointee(cfg));
    let stats = Arc::new(StatsTracker::new());
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        DaemonLifecycle::new(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_warm_from_empty() {
    let testdb = require_test_db!();
    seed_project_with_symbols(testdb.pool(), "lazy_warm_test", &["alpha_unique_function"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().to_path_buf();
    let expected_path = trie_path(&data_dir, "symbols", &slugify("lazy_warm_test"));
    assert!(
        !expected_path.exists(),
        "trie file must not exist before the call: {}",
        expected_path.display()
    );

    let ctx = build_ctx_with_data_dir(Arc::new(testdb.pool().clone()), data_dir);
    let result = tool_fuzzy_symbol_search::run(
        &ctx,
        FuzzySymbolSearchParams {
            query: "alpha_unique_function".to_string(),
            project: "lazy_warm_test".to_string(),
            max_distance: Some(0),
            limit: Some(10),
        },
    )
    .await
    .expect("tool call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");
    let terms: Vec<&str> = val["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .filter_map(|h| h.get("term").and_then(|v| v.as_str()))
        .collect();
    assert!(
        terms.iter().any(|t| t == &"alpha_unique_function"),
        "lazy-warmed trie must surface the PG symbol; got {terms:?}"
    );
    assert!(
        expected_path.exists(),
        "trie file must exist after the lazy warm: {}",
        expected_path.display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_trie_beats_stale_pg() {
    let testdb = require_test_db!();
    // PG carries the stale symbol; the trie carries the authoritative one.
    seed_project_with_symbols(testdb.pool(), "trie_beats_pg", &["pg_old_func"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().to_path_buf();
    let trie_file = trie_path(&data_dir, "symbols", &slugify("trie_beats_pg"));

    // Pre-populate the trie so lazy-warm DOES NOT run on the first
    // tool call.
    let (idx, _recovery) =
        FuzzyIndex::<SymbolValue>::open_or_create(&trie_file).expect("open_or_create");
    idx.upsert(
        "trie_authoritative_func",
        SymbolValue {
            file_id: 999,
            kind: "function".to_string(),
            visibility: "public".to_string(),
            line: 1,
        },
    )
    .expect("upsert");
    drop(idx); // release the handle so the tool can re-open cleanly.
    assert!(trie_file.exists());

    let ctx = build_ctx_with_data_dir(Arc::new(testdb.pool().clone()), data_dir);
    let result = tool_fuzzy_symbol_search::run(
        &ctx,
        FuzzySymbolSearchParams {
            query: "authoritative".to_string(),
            project: "trie_beats_pg".to_string(),
            max_distance: Some(20),
            limit: Some(20),
        },
    )
    .await
    .expect("tool call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");
    let terms: Vec<&str> = val["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .filter_map(|h| h.get("term").and_then(|v| v.as_str()))
        .collect();
    assert!(
        terms.iter().any(|t| t == &"trie_authoritative_func"),
        "trie's symbol must appear; got {terms:?}"
    );
    assert!(
        !terms.iter().any(|t| t == &"pg_old_func"),
        "tool must NOT consult PG once the trie exists; got {terms:?}"
    );
}
