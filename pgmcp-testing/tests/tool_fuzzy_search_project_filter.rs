//! P14.3 — `tool_fuzzy_symbol_search` / `tool_fuzzy_path_search`
//! per-project isolation via the persistent `FuzzyIndex`.
//!
//! Pre-P14.3 this test asserted "the PG project-filter SQL works".
//! Post-P14.3 the tools no longer touch PG at the query path (only
//! during the first-call lazy warm), and project isolation is a
//! property of the per-project trie file living under
//! `<data_dir>/fuzzy/symbols/<slug>/symbols.artrie`. The new
//! assertion shape: seed two projects in PG, point each test at a
//! fresh tempdir-rooted `data_dir` so lazy warm pulls them
//! correctly, call the tool with project A, assert project B's
//! symbols are absent.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::{FuzzyPathSearchParams, FuzzySymbolSearchParams};
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::{tool_fuzzy_path_search, tool_fuzzy_symbol_search};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

async fn seed_project_with_symbols(
    pool: &sqlx::PgPool,
    project_name: &str,
    file_relpath: &str,
    symbol_names: &[&str],
) {
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
    .bind(format!("/ws/{project_name}/proj/{file_relpath}"))
    .bind(file_relpath)
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
            "INSERT INTO file_symbols (file_id, name, kind, visibility, line_start, line_end) \
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
async fn symbol_search_with_project_excludes_other_projects_symbols() {
    let testdb = require_test_db!();
    seed_project_with_symbols(
        testdb.pool(),
        "filter_test_alpha",
        "src/lib.rs",
        &["alpha_unique_function", "shared_function"],
    )
    .await;
    seed_project_with_symbols(
        testdb.pool(),
        "filter_test_beta",
        "src/lib.rs",
        &["beta_unique_function", "shared_function"],
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx_with_data_dir(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());
    let result = tool_fuzzy_symbol_search::run(
        &ctx,
        FuzzySymbolSearchParams {
            query: "unique_function".to_string(),
            project: "filter_test_alpha".to_string(),
            max_distance: Some(8),
            limit: Some(50),
        },
    )
    .await
    .expect("call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");
    let hits = val.get("hits").and_then(|h| h.as_array()).expect("hits");
    let terms: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("term").and_then(|v| v.as_str()))
        .collect();
    assert!(
        terms.iter().any(|t| t.contains("alpha_unique_function")),
        "must include alpha's unique symbol; got {terms:?}"
    );
    assert!(
        !terms.iter().any(|t| t.contains("beta_unique_function")),
        "must exclude beta's unique symbol; got {terms:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn path_search_with_project_excludes_other_projects_paths() {
    let testdb = require_test_db!();
    seed_project_with_symbols(
        testdb.pool(),
        "path_filter_alpha",
        "src/alpha_only.rs",
        &["a"],
    )
    .await;
    seed_project_with_symbols(
        testdb.pool(),
        "path_filter_beta",
        "src/beta_only.rs",
        &["b"],
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx_with_data_dir(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());
    let result = tool_fuzzy_path_search::run(
        &ctx,
        FuzzyPathSearchParams {
            query: "src/alpha_only.rs".to_string(),
            project: "path_filter_alpha".to_string(),
            max_distance: Some(0),
            limit: Some(50),
        },
    )
    .await
    .expect("call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");
    let hits = val.get("hits").and_then(|h| h.as_array()).expect("hits");
    let paths: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("path").and_then(|v| v.as_str()))
        .collect();
    assert!(
        paths.iter().any(|p| p == &"src/alpha_only.rs"),
        "must include alpha's path; got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p == &"src/beta_only.rs"),
        "must exclude beta's path; got {paths:?}"
    );
}
