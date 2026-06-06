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
pub fn context_with_pool(pool: sqlx::PgPool) -> SystemContext {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    )
}

/// Build an `McpServer` whose `db` is the given real `PgPool` and whose
/// embedder is the deterministic test backend (no model download, no GPU).
pub fn server_with_pool(pool: sqlx::PgPool) -> McpServer {
    McpServer::new(context_with_pool(pool))
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

/// Seed a `file_symbols` row, returning its id. `visibility` is e.g.
/// `Some("public")` to exercise the public-API-reachable severity bump in the
/// lock-order analysis; `None` leaves it NULL. `end_line` is pinned to
/// `start_line` (the analyzers key on identity + order, not span). Idempotent on
/// the `(file_id, kind, name, start_line)` unique key.
pub async fn seed_file_symbol(
    pool: &sqlx::PgPool,
    file_id: i64,
    name: &str,
    kind: &str,
    start_line: i32,
    visibility: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO file_symbols (file_id, name, kind, start_line, end_line, visibility) \
         VALUES ($1, $2, $3, $4, $4, $5) \
         ON CONFLICT (file_id, kind, name, start_line) \
             DO UPDATE SET visibility = EXCLUDED.visibility \
         RETURNING id",
    )
    .bind(file_id)
    .bind(name)
    .bind(kind)
    .bind(start_line)
    .bind(visibility)
    .fetch_one(pool)
    .await
    .expect("file_symbol")
}

/// Seed one ordered `sync_ops` row for a symbol (the synchronization skeleton
/// the deadlock analyzers read). `op_kind` / `resource_kind` / `paradigm` are
/// the closed-vocab DB strings (e.g. `acquire`/`mutex`/`lock`,
/// `recv`/`channel`/`message`). `resource_confidence` is pinned to 0.9 (above
/// the 0.3 analysis floor) so seeded edges are never dropped. Idempotent on
/// `(symbol_id, seq)`.
#[allow(clippy::too_many_arguments)]
pub async fn seed_sync_ops(
    pool: &sqlx::PgPool,
    symbol_id: i64,
    seq: i32,
    op_kind: &str,
    resource_key: &str,
    resource_kind: &str,
    paradigm: &str,
    line: i32,
) {
    sqlx::query(
        "INSERT INTO sync_ops \
             (symbol_id, seq, op_kind, resource_key, resource_kind, paradigm, \
              nesting_depth, guard_id, resource_confidence, line) \
         VALUES ($1, $2, $3, $4, $5, $6, 0, NULL, 0.9, $7) \
         ON CONFLICT (symbol_id, seq) DO UPDATE \
             SET op_kind = EXCLUDED.op_kind, resource_key = EXCLUDED.resource_key, \
                 resource_kind = EXCLUDED.resource_kind, paradigm = EXCLUDED.paradigm, \
                 line = EXCLUDED.line",
    )
    .bind(symbol_id)
    .bind(seq)
    .bind(op_kind)
    .bind(resource_key)
    .bind(resource_kind)
    .bind(paradigm)
    .bind(line)
    .execute(pool)
    .await
    .expect("sync_op");
}

/// Seed a resolved call edge (`source_symbol → target_symbol`) into
/// `symbol_references`, the relation the lock-order analyzer reads for
/// interprocedural lock inlining. `resolution_confidence` must be ≥ 0.5 for the
/// inliner to follow the edge (`resolved_call_edges_for_project`'s floor).
/// Idempotent on `(source_file_id, source_line, target_raw, ref_kind)`.
pub async fn seed_symbol_references(
    pool: &sqlx::PgPool,
    source_file_id: i64,
    source_symbol_id: i64,
    target_symbol_id: i64,
    target_raw: &str,
    source_line: i32,
    resolution_confidence: f32,
) {
    sqlx::query(
        "INSERT INTO symbol_references \
             (source_file_id, source_symbol_id, target_symbol_id, target_raw, \
              ref_kind, source_line, resolution_confidence) \
         VALUES ($1, $2, $3, $4, 'call', $5, $6) \
         ON CONFLICT (source_file_id, source_line, target_raw, ref_kind) DO UPDATE \
             SET source_symbol_id = EXCLUDED.source_symbol_id, \
                 target_symbol_id = EXCLUDED.target_symbol_id, \
                 resolution_confidence = EXCLUDED.resolution_confidence",
    )
    .bind(source_file_id)
    .bind(source_symbol_id)
    .bind(target_symbol_id)
    .bind(target_raw)
    .bind(source_line)
    .bind(resolution_confidence)
    .execute(pool)
    .await
    .expect("symbol_reference");
}
