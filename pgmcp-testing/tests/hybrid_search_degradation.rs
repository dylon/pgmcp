//! Bug-2 regression: `hybrid_search` degrades gracefully when a leg fails
//! instead of failing the whole tool.
//!
//! Uses `MockDbClient` to force the BM25/text leg (and optionally the semantic
//! leg) to error deterministically, then asserts the tool still returns `Ok`
//! with `degraded: true` + a per-leg `leg_status`, and surfaces whatever the
//! surviving leg produced. The previous fail-fast `?` aborted the entire tool
//! when the text leg hit a Postgres `statement_timeout` under load.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::db::queries::SearchResult;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::HybridSearchParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_hybrid_search::tool_hybrid_search;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};

fn ctx_with(db: Arc<dyn DbClient>) -> SystemContext {
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        Arc::new(StatsTracker::new()),
        config,
        Arc::new(LogBroadcaster::new()),
        Arc::new(TaskStore::new()),
        DaemonLifecycle::new(),
    )
}

fn one_semantic_hit() -> SearchResult {
    SearchResult {
        path: "/ws/degr/src/lib.rs".to_string(),
        relative_path: "src/lib.rs".to_string(),
        language: "rust".to_string(),
        chunk_content: "fn handle_request() {}".to_string(),
        start_line: 1,
        end_line: 1,
        score: Some(0.9),
        project_name: "degr".to_string(),
        chunk_id: None,
    }
}

fn parse(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    serde_json::from_str(&text).expect("json")
}

fn params() -> HybridSearchParams {
    HybridSearchParams {
        query: "handle request".to_string(),
        project: None,
        language: None,
        limit: Some(10),
        bm25_weight: Some(0.5),
        semantic_weight: Some(0.5),
        dedupe_worktrees: Some(false),
        // No project + zero WFST weight → the third leg never runs (skipped),
        // isolating the text/semantic degradation behavior under test.
        wfst_lm_weight: Some(0.0),
        max_query_edit_distance: Some(2),
        return_type_tags: None,
        effects: None,
        scope_kind: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn degrades_when_text_leg_fails_but_returns_semantic_hits() {
    let mut mock = MockDbClient::new();
    mock.text_search_bounded_fails = true;
    mock.semantic_search_results = vec![one_semantic_hit()];
    let ctx = ctx_with(Arc::new(mock));

    let result = tool_hybrid_search(&ctx, params())
        .await
        .expect("a single leg's error must NOT fail the whole tool");
    let val = parse(&result);

    assert_eq!(val["degraded"].as_bool(), Some(true));
    assert_eq!(val["leg_status"]["text"].as_str(), Some("error"));
    assert_eq!(val["leg_status"]["semantic"].as_str(), Some("ok"));
    assert!(
        val["fused_count"].as_u64().unwrap_or(0) >= 1,
        "the surviving semantic leg's hit must still be returned; got {val:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn isolated_text_leg_failure_returns_empty_not_error() {
    // The exact tracker repro: semantic_weight=0 + wfst_lm_weight=0 isolates
    // the text leg. When it errors, the tool must return Ok + empty +
    // degraded:true, NOT a hard MCP error (the previous behavior).
    let mut mock = MockDbClient::new();
    mock.text_search_bounded_fails = true;
    let ctx = ctx_with(Arc::new(mock));

    let mut p = params();
    p.semantic_weight = Some(0.0);
    let result = tool_hybrid_search(&ctx, p)
        .await
        .expect("isolated text-leg failure must degrade, not hard-error");
    let val = parse(&result);

    assert_eq!(val["degraded"].as_bool(), Some(true));
    assert_eq!(val["leg_status"]["text"].as_str(), Some("error"));
    assert_eq!(val["leg_status"]["semantic"].as_str(), Some("skipped"));
    assert_eq!(val["fused_count"].as_u64(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn both_legs_failing_still_returns_ok_degraded() {
    let mut mock = MockDbClient::new();
    mock.text_search_bounded_fails = true;
    mock.semantic_search_fails = true;
    let ctx = ctx_with(Arc::new(mock));

    let result = tool_hybrid_search(&ctx, params())
        .await
        .expect("both legs failing must still return Ok + degraded, not hard-error");
    let val = parse(&result);

    assert_eq!(val["degraded"].as_bool(), Some(true));
    assert_eq!(val["leg_status"]["text"].as_str(), Some("error"));
    assert_eq!(val["leg_status"]["semantic"].as_str(), Some("error"));
    assert_eq!(val["fused_count"].as_u64(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn healthy_legs_are_not_degraded() {
    let mut mock = MockDbClient::new();
    mock.semantic_search_results = vec![one_semantic_hit()];
    let ctx = ctx_with(Arc::new(mock));

    let result = tool_hybrid_search(&ctx, params())
        .await
        .expect("hybrid_search");
    let val = parse(&result);

    assert_eq!(val["degraded"].as_bool(), Some(false));
    assert_eq!(val["leg_status"]["text"].as_str(), Some("ok"));
    assert_eq!(val["leg_status"]["semantic"].as_str(), Some("ok"));
    assert_eq!(val["leg_status"]["wfst"].as_str(), Some("skipped"));
}
