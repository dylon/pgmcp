//! Real-DB oracle for the software pattern / anti-pattern catalog.
//!
//! Verifies the four `kind` values are populated by `seed_catalog` /
//! `warm_pattern_catalog` and exposed through the MCP catalog tools:
//!
//! - `pattern_catalog_stats` reports nonzero counts for every kind.
//! - `list_software_patterns` filtered by each kind returns rows.
//! - `software_pattern_search` returns at least one hit (even with the
//!   deterministic embedder, the chunk-level kNN must return some row).
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::Value;

fn extract_json(call_result: &rmcp::model::CallToolResult) -> Value {
    for content in &call_result.content {
        if let rmcp::model::RawContent::Text(text_content) = &content.raw {
            return serde_json::from_str::<Value>(&text_content.text)
                .expect("tool emitted invalid JSON");
        }
    }
    panic!("tool returned no Text content block");
}

#[tokio::test(flavor = "multi_thread")]
async fn catalog_stats_reports_all_four_kinds() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // Force the lazy-seed path by calling any pattern tool first.
    let _ = server
        .call_tool_cli("pattern_catalog_stats", serde_json::json!({}))
        .await
        .expect("pattern_catalog_stats call");

    let result = server
        .call_tool_cli("pattern_catalog_stats", serde_json::json!({}))
        .await
        .expect("second pattern_catalog_stats call");
    let body = extract_json(&result);
    let stats = body
        .get("stats")
        .expect("stats key present in pattern_catalog_stats output");

    for key in ["patterns", "anti_patterns", "principles", "code_smells"] {
        let n = stats
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("stats.{key} missing or not an integer"));
        assert!(
            n > 0,
            "stats.{key} expected > 0 after seed; got {n}. Full stats: {stats}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_patterns_kind_filter_returns_rows_for_each_kind() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // Trigger lazy seed before the asserts.
    let _ = server
        .call_tool_cli("pattern_catalog_stats", serde_json::json!({}))
        .await
        .expect("warm-up call");

    for kind in ["pattern", "anti_pattern", "principle", "code_smell"] {
        let result = server
            .call_tool_cli(
                "list_software_patterns",
                serde_json::json!({"kind": kind, "limit": 5}),
            )
            .await
            .unwrap_or_else(|e| panic!("list_software_patterns(kind={kind}) failed: {e}"));
        let body = extract_json(&result);
        let rows = body
            .get("patterns")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("patterns array missing for kind={kind}"));
        assert!(
            !rows.is_empty(),
            "expected at least 1 row for kind={kind}, got 0. body: {body}"
        );
        for row in rows {
            let row_kind = row
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("row missing kind: {row}"));
            assert_eq!(
                row_kind, kind,
                "list_software_patterns(kind={kind}) returned row with kind={row_kind}: {row}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn software_pattern_search_returns_results() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let _ = server
        .call_tool_cli("pattern_catalog_stats", serde_json::json!({}))
        .await
        .expect("warm-up call");

    // Even with the deterministic embedder, the kNN over ~1400 chunks
    // returns *some* nearest neighbours. Asserting nonempty results is a
    // light smoke check that the seed → embed → query path is wired.
    let result = server
        .call_tool_cli(
            "software_pattern_search",
            serde_json::json!({"query": "isolate state with messages", "limit": 5}),
        )
        .await
        .expect("software_pattern_search call");
    let body = extract_json(&result);
    let rows = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results array present");
    assert!(
        !rows.is_empty(),
        "expected at least 1 search result, got 0. body: {body}"
    );
}
