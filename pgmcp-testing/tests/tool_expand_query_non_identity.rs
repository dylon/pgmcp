//! P13.4 — tool_expand_query_to_phonetic_pattern is not a stub.
//!
//! The old stub returned `expanded == input`. The real
//! implementation runs `expand_phonetic_alternatives_char` via
//! `PgmcpPhonetics::expand_to_pattern` and returns a regex-style
//! alternation pattern that can match phonetic variants.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::ExpandQueryToPhoneticPatternParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_expand_query_to_phonetic_pattern;
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
async fn ph_to_f_term_produces_alternation_pattern() {
    let testdb = require_test_db!();
    let ctx = build_ctx(Arc::new(testdb.pool().clone()));

    // "phone" is the canonical English ph→f rule target; the
    // expanded pattern must include alternation.
    let result = tool_expand_query_to_phonetic_pattern::run(
        &ctx,
        ExpandQueryToPhoneticPatternParams {
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

    let expanded = val
        .get("expanded")
        .and_then(|v| v.as_str())
        .expect("expanded field");
    assert!(!expanded.is_empty());
    // Stub returned `expanded == "phone"`. Real implementation must
    // produce a different string (alternation or normalized form).
    assert_ne!(
        expanded, "phone",
        "expansion must transform the input; got identity: {expanded}"
    );
}
