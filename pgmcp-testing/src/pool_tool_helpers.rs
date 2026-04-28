//! Shared helpers for the `tool_*_integration.rs` test files.
//!
//! Each of the 5 tool-integration test files (graph, architecture,
//! prediction, scorecard, topic) uses the same pattern: wrap a test
//! `PgPool` in a `SystemContext` + build an `McpServer`, then seed a
//! minimal project + file so the tools have something to query. The
//! helpers below keep those files short and focused on the tool
//! assertions themselves.

use std::sync::Arc;

use arc_swap::ArcSwap;

use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;

use crate::fixtures::test_config;
use crate::mocks::DeterministicEmbeddingBackend;

/// Build an `McpServer` whose `db` is the given real `PgPool` and whose
/// embedder is the deterministic test backend (no model download, no GPU).
pub fn server_with_pool(pool: sqlx::PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    McpServer::new(ctx)
}

/// Upsert a project row, returning its id.
pub async fn seed_project(pool: &sqlx::PgPool, name: &str, path: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

/// Seed a single indexed_files row with trivial content — enough to make
/// the pool()-using tools return a result instead of an empty envelope.
pub async fn seed_file(pool: &sqlx::PgPool, project_id: i32, path: &str, rel: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 10, 'fn f() {}', 1, 1, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content_hash = 1 RETURNING id"
    )
    .bind(project_id)
    .bind(path)
    .bind(rel)
    .fetch_one(pool)
    .await
    .expect("file")
}
