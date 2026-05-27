//! G10 integration tests: the grep-family tools rebuilt on liblevenshtein's
//! `TokenGrep` (positional, structured, parallel — no transient `DynamicDawgChar`).
//!
//! - `tool_token_grep`: structured token-pattern fuzzy grep with per-token detail.
//! - `tool_fuzzy_grep`: positional fuzzy grep with byte spans + edit distance.
//!
//! These tools only read `ctx.stats()` (the haystack is caller-supplied), but
//! constructing a `SystemContext` needs a `DbClient`, so they go through
//! `require_test_db!()` and self-skip without a CREATEDB-capable test DB.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::{FuzzyGrepParams, TokenGrepParams};
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::{tool_fuzzy_grep, tool_token_grep};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

fn build_ctx(db: Arc<dyn DbClient>) -> SystemContext {
    SystemContext::production(
        db,
        EmbedSource::backend(Arc::new(DeterministicEmbeddingBackend::new(1024))),
        Arc::new(StatsTracker::new()),
        Arc::new(ArcSwap::from_pointee(Config::default())),
        Arc::new(LogBroadcaster::new()),
        Arc::new(TaskStore::new()),
        DaemonLifecycle::new(),
    )
}

fn result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    serde_json::from_str(&text).expect("json")
}

#[tokio::test(flavor = "multi_thread")]
async fn token_grep_finds_fuzzy_token_with_positions() {
    let testdb = require_test_db!();
    let ctx = build_ctx(Arc::new(testdb.pool().clone()));

    // "recieve" (typo) within distance 2 of "receive" in the haystack lines.
    let result = tool_token_grep::run(
        &ctx,
        TokenGrepParams {
            query: "recieve".to_string(),
            haystack: vec![
                "fn receive_event(&self)".to_string(),
                "fn render(&self)".to_string(),
            ],
            max_distance: Some(2),
        },
    )
    .await
    .expect("token_grep");
    let val = result_json(&result);
    let matches = val["matches"].as_array().expect("matches array");
    assert!(
        !matches.is_empty(),
        "token_grep should fuzzily match 'receive' in the haystack; got {val}"
    );
    // Positional: each match carries byte offsets + a distance.
    assert!(matches[0].get("byte_start").is_some());
    assert!(matches[0].get("total_distance").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn fuzzy_grep_reports_positional_matches() {
    let testdb = require_test_db!();
    let ctx = build_ctx(Arc::new(testdb.pool().clone()));

    let result = tool_fuzzy_grep::run(
        &ctx,
        FuzzyGrepParams {
            query: "colour".to_string(),
            haystack: vec!["let color = 1;".to_string(), "let count = 2;".to_string()],
            max_distance: Some(2),
        },
    )
    .await
    .expect("fuzzy_grep");
    let val = result_json(&result);
    let matches = val["matches"].as_array().expect("matches array");
    assert!(
        !matches.is_empty(),
        "fuzzy_grep should match 'color' for query 'colour'; got {val}"
    );
    assert!(matches[0].get("byte_start").is_some());
    assert!(matches[0].get("distance").is_some());
}
