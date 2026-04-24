//! Numerical oracle tests for the Reciprocal Rank Fusion formula
//! used by `hybrid_search` and the higher-level RRF merging behaviour.
//!
//! Two layers of test:
//!
//! 1. **Pure formula** — `pgmcp::mcp::tools::tool_hybrid_search::rrf_score`
//!    is exercised directly with hand-computed expected values. Catches
//!    algebraic regressions (k change, weight inversion, off-by-one on
//!    `rank`).
//!
//! 2. **End-to-end merge** — drive the whole `hybrid_search` MCP tool
//!    via `MockDbClient` with pinned text and semantic results, then
//!    parse the JSON output and assert the fused order matches the
//!    by-hand RRF tally.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::{SearchResult, TextSearchResult};
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_hybrid_search::{RRF_K, rrf_score};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};

const FORMULA_TOL: f64 = 1e-12;

// ============================================================================
// Layer 1 — pure formula
// ============================================================================

#[test]
fn rrf_score_at_rank_zero_with_unit_weight_equals_one_over_k_plus_one() {
    // weight=1, k=60, rank=0 → 1 / (60 + 0 + 1) = 1/61 ≈ 0.016393…
    let s = rrf_score(1.0, RRF_K, 0);
    assert!(
        (s - 1.0 / 61.0).abs() < FORMULA_TOL,
        "rrf_score(1, 60, 0) = {s}, expected 1/61"
    );
}

#[test]
fn rrf_score_decreases_monotonically_with_rank() {
    let mut prev = f64::INFINITY;
    for rank in 0..50 {
        let s = rrf_score(1.0, RRF_K, rank);
        assert!(
            s < prev,
            "rrf_score should decrease with rank: at rank {rank} got {s} >= prev {prev}"
        );
        prev = s;
    }
}

#[test]
fn rrf_score_scales_linearly_in_weight() {
    // 2× weight → 2× score; verify the formula is `weight / (k + rank + 1)`,
    // not e.g. `weight + 1 / (k + rank + 1)` or some other corruption.
    for rank in [0, 1, 5, 20, 100] {
        let unit = rrf_score(1.0, RRF_K, rank);
        let half = rrf_score(0.5, RRF_K, rank);
        let two = rrf_score(2.0, RRF_K, rank);
        assert!(
            (half * 2.0 - unit).abs() < FORMULA_TOL,
            "0.5 weight × 2 should equal 1.0 weight at rank {rank}"
        );
        assert!(
            (two - 2.0 * unit).abs() < FORMULA_TOL,
            "2.0 weight should equal 2× 1.0 weight at rank {rank}"
        );
    }
}

#[test]
fn rrf_constant_k_is_60_per_cormack_2009() {
    // The literature (Cormack et al., SIGIR 2009) prescribes k=60.
    // If this constant ever drifts, every blended score changes — pin it.
    assert!((RRF_K - 60.0).abs() < FORMULA_TOL, "RRF_K = {RRF_K}");
}

// ============================================================================
// Layer 2 — end-to-end merge through the real hybrid_search tool
// ============================================================================

/// Build a fully-wired McpServer with mocked DB and embedding backend.
fn server_with_results(
    semantic_results: Vec<SearchResult>,
    text_results: Vec<TextSearchResult>,
) -> McpServer {
    let mut mock = MockDbClient::new();
    mock.semantic_search_results = semantic_results;
    mock.text_search_results = text_results;
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let embed_source = EmbedSource::backend(embed_backend);
    let ctx =
        SystemContext::production(db, embed_source, stats, config, log_broadcaster, task_store);
    McpServer::new(ctx)
}

fn semantic_hit(path: &str, line: i32) -> SearchResult {
    SearchResult {
        path: format!("/ws/p/{}", path),
        relative_path: path.into(),
        language: "rust".into(),
        chunk_content: format!("semantic body for {path}"),
        start_line: line,
        end_line: line,
        score: Some(0.9),
        project_name: "p".into(),
    }
}

fn text_hit(path: &str) -> TextSearchResult {
    TextSearchResult {
        path: format!("/ws/p/{}", path),
        relative_path: path.into(),
        language: "rust".into(),
        content: Some(format!("text body for {path}")),
        rank: Some(0.7),
    }
}

/// Extract the first text-Content's payload from a tool result.
fn text_of(result: &rmcp::model::CallToolResult) -> String {
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

/// Pull (path, rrf_score) pairs in result order from the tool's JSON
/// payload. The tool serializes rrf_score as a string (e.g. "0.016393")
/// — re-parse to f64.
fn parse_results(payload: &str) -> Vec<(String, f64)> {
    let v: serde_json::Value = serde_json::from_str(payload).expect("parse json");
    v["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| {
            let path = r["relative_path"]
                .as_str()
                .expect("relative_path")
                .to_string();
            let score: f64 = r["rrf_score"]
                .as_str()
                .expect("rrf_score string")
                .parse()
                .expect("rrf_score parses");
            (path, score)
        })
        .collect()
}

#[tokio::test]
async fn rrf_merges_two_sources_with_known_final_rank() {
    // Three semantic results (ranks 0,1,2) and three text results
    // (ranks 0,1,2). With weights 0.5/0.5 every entry's RRF score is
    // exactly `0.5 / (60 + rank + 1)`. Because the tool keys text and
    // semantic separately by source tag, the same path appearing on
    // both sides produces two entries that don't merge — we assert
    // that exact behaviour (no silent merging).
    let server = server_with_results(
        vec![
            semantic_hit("alpha.rs", 1),
            semantic_hit("beta.rs", 1),
            semantic_hit("gamma.rs", 1),
        ],
        vec![text_hit("delta.rs"), text_hit("epsilon.rs")],
    );

    let result = server
        .call_tool_cli("hybrid_search", serde_json::json!({"query": "q"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    let results = parse_results(&payload);

    // 5 entries total: 3 semantic + 2 text — none collide on the
    // (source, key) tuple.
    assert_eq!(results.len(), 5, "5 distinct entries; payload:\n{payload}");

    // Hand-tally per-entry RRF score with weight=0.5, k=60.
    let expected = |rank: usize| 0.5 / (60.0 + rank as f64 + 1.0);

    // Top three are rank-0 entries from each source. Within rank-0
    // there's a tie between alpha.rs and delta.rs — accept either order.
    let top_three: std::collections::BTreeSet<&str> =
        results.iter().take(3).map(|(p, _)| p.as_str()).collect();
    assert!(
        top_three.contains("alpha.rs") && top_three.contains("delta.rs"),
        "top-3 should contain both rank-0 entries, got {top_three:?}"
    );

    // Verify each parsed score matches the formula at the entry's rank
    // within its own source. Build the rank lookup.
    let semantic_ranks: std::collections::HashMap<&str, usize> =
        [("alpha.rs", 0), ("beta.rs", 1), ("gamma.rs", 2)]
            .into_iter()
            .collect();
    let text_ranks: std::collections::HashMap<&str, usize> =
        [("delta.rs", 0), ("epsilon.rs", 1)].into_iter().collect();

    for (path, score) in &results {
        let rank = semantic_ranks
            .get(path.as_str())
            .or_else(|| text_ranks.get(path.as_str()))
            .copied()
            .unwrap_or_else(|| panic!("unknown result path {path}"));
        let want = expected(rank);
        // Tool serializes with 6-decimal precision — accept a 1e-6 tolerance.
        assert!(
            (score - want).abs() < 1e-6,
            "RRF score for {path} (rank {rank}) = {score}, expected {want}"
        );
    }
}

#[tokio::test]
async fn rrf_weight_inversion_demotes_corresponding_source() {
    // With bm25_weight=0.99 and semantic_weight=0.01, the rank-0 text
    // result should outscore *every* semantic result (including
    // rank-0 semantic). Pin that.
    let server = server_with_results(
        vec![semantic_hit("sem_top.rs", 1), semantic_hit("sem_mid.rs", 1)],
        vec![text_hit("text_top.rs"), text_hit("text_mid.rs")],
    );

    let result = server
        .call_tool_cli(
            "hybrid_search",
            serde_json::json!({
                "query": "q",
                "bm25_weight": 0.99,
                "semantic_weight": 0.01,
            }),
        )
        .await
        .expect("tool call");
    let payload = text_of(&result);
    let results = parse_results(&payload);
    assert!(!results.is_empty(), "no results in payload:\n{payload}");
    assert_eq!(
        results[0].0, "text_top.rs",
        "with bm25 weight 99×, top result must be text_top.rs but got {:?}",
        results[0].0
    );
}

#[tokio::test]
async fn rrf_handles_text_only_input_returning_text_results_in_order() {
    let server = server_with_results(
        vec![],
        vec![
            text_hit("first.rs"),
            text_hit("second.rs"),
            text_hit("third.rs"),
        ],
    );

    let result = server
        .call_tool_cli("hybrid_search", serde_json::json!({"query": "q"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    let results = parse_results(&payload);
    assert_eq!(results.len(), 3, "3 text results; payload:\n{payload}");
    let order: Vec<&str> = results.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        order,
        vec!["first.rs", "second.rs", "third.rs"],
        "text-only input must preserve source order"
    );
}

#[tokio::test]
async fn rrf_handles_semantic_only_input_returning_semantic_results_in_order() {
    let server = server_with_results(
        vec![
            semantic_hit("first.rs", 1),
            semantic_hit("second.rs", 1),
            semantic_hit("third.rs", 1),
        ],
        vec![],
    );

    let result = server
        .call_tool_cli("hybrid_search", serde_json::json!({"query": "q"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    let results = parse_results(&payload);
    assert_eq!(results.len(), 3);
    let order: Vec<&str> = results.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(order, vec!["first.rs", "second.rs", "third.rs"]);
}
