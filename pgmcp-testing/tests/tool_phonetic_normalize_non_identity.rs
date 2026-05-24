//! P13.4 — tool_phonetic_normalize is not a stub.
//!
//! The old stub returned the input verbatim under
//! `articulatory_self_distance: 0`. The real implementation runs
//! `PgmcpPhonetics::normalize` (the embedded English Zompist rules)
//! and returns `normalized` AND `expanded_pattern`. This test
//! catches regression to the identity behavior.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::PhoneticNormalizeParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_phonetic_normalize;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

fn build_ctx(db: Arc<dyn DbClient>) -> SystemContext {
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
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
async fn response_includes_normalized_and_expanded_fields() {
    let testdb = require_test_db!();
    let ctx = build_ctx(Arc::new(testdb.pool().clone()));

    let result = tool_phonetic_normalize::run(
        &ctx,
        PhoneticNormalizeParams {
            term: "phone".to_string(),
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

    // P13.4 contract: tool must surface BOTH `normalized` and
    // `expanded_pattern`. Stub returned neither.
    assert!(
        val.get("normalized").is_some(),
        "`normalized` field missing: {val:#}"
    );
    assert!(
        val.get("expanded_pattern").is_some(),
        "`expanded_pattern` field missing: {val:#}"
    );
    assert!(
        val.get("language").is_some(),
        "`language` field missing: {val:#}"
    );
    // Stub returned `articulatory_self_distance: 0.0` as its sole
    // signal. The real implementation MUST NOT carry that field.
    assert!(
        val.get("articulatory_self_distance").is_none(),
        "stub field `articulatory_self_distance` must be gone: {val:#}"
    );
}
