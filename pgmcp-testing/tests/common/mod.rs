//! Shared helpers for the per-tool oracle tests.
//!
//! Each test file under `tests/` is its own crate, but Cargo allows
//! a `common` module accessed via `mod common;` from each top-level
//! test file. This avoids duplicating the McpServer + SystemContext
//! wiring across the dozen+ Phase G–J oracles.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};
use sqlx::PgPool;

/// Build an `McpServer` whose DbClient is a populated `MockDbClient`.
/// Use for tests that don't touch real Postgres.
#[allow(dead_code)]
pub fn server_with_mock(mock: MockDbClient) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(mock);
    server_with_db_arc(db, test_config())
}

/// Build an `McpServer` whose DbClient is a real `PgPool`. Use for
/// tests against `db_harness::TestDatabase`.
#[allow(dead_code)]
pub fn server_with_pool(pool: PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    server_with_db_arc(db, Config::default())
}

fn context_with_db_arc(db: Arc<dyn DbClient>, cfg: Config) -> SystemContext {
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(cfg));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        {
            let __l = pgmcp::daemon_state::DaemonLifecycle::new();
            __l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
            __l
        },
    )
}

fn server_with_db_arc(db: Arc<dyn DbClient>, cfg: Config) -> McpServer {
    McpServer::new(context_with_db_arc(db, cfg))
}

/// Build a `SystemContext` over a real `PgPool` with the deterministic 1024-d
/// embedder — for tests that exercise context-level helpers (e.g. the
/// `tool_catalog` warm-up embed) directly rather than through `McpServer`.
#[allow(dead_code)]
pub fn context_with_pool(pool: PgPool) -> SystemContext {
    context_with_db_arc(Arc::new(pool), Config::default())
}

/// Pull the first text-Content payload out of an MCP tool result.
#[allow(dead_code)]
pub fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}
