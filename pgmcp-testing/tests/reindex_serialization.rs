//! Tier 2 verification — confirm `tool_reindex` honors the
//! `SystemContext::reindex_lock` and refuses concurrent invocations.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn tool_reindex_rejects_concurrent_invocation() {
    let db = require_test_db!();
    let pool: sqlx::PgPool = db.pool().clone();

    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let lifecycle = DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let db_client: Arc<dyn DbClient> = Arc::new(pool);
    let ctx = SystemContext::production(
        db_client,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );

    // Simulate a reindex already in progress by acquiring the lock
    // manually. `try_lock` should fail and the tool should surface the
    // conflict cleanly.
    let _guard = ctx
        .reindex_lock()
        .try_lock()
        .expect("first try_lock must succeed on a fresh context");

    let result = pgmcp::mcp::tools::tool_reindex::tool_reindex(&ctx).await;
    let err = result.expect_err("reindex must refuse while the lock is held");
    let msg = err.message.to_string();
    assert!(
        msg.contains("Another reindex is already in progress"),
        "unexpected error message: {msg}",
    );
}
