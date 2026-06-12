//! v31 integration coverage for `conversation_search` (the A2A/coordination
//! convenience wrapper over `memory_unified_search`) and the `search_mandates`
//! semantic/hybrid legs.
//!
//! These satisfy the `query_inventory_vs_coverage` gate (every dispatched tool
//! needs a `call_tool_cli` test) and exercise the real SQL against the unified
//! matview + the `durable_mandates.embedding` column. The harness embedder is
//! the `DeterministicEmbeddingBackend(1024)`, so the embed legs run end-to-end.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

#[tokio::test]
async fn tool_conversation_search_executes_against_unified_graph() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // A2A/coordination tables are empty in a fresh DB; the point is that the
    // pinned-node_type unified search executes and returns a well-formed
    // envelope (the v31 node types must be registered, else this 400s).
    let result = server
        .call_tool_cli(
            "conversation_search",
            json!({"query": "worktree negotiation"}),
        )
        .await
        .expect("conversation_search must not error against the unified matview");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("conversation_search body must be JSON");

    assert!(v["count"].is_number(), "count must be present");
    assert!(v["results"].is_array(), "results must be an array");
    let pinned = v["node_types"].as_array().expect("node_types echoed");
    let pinned: Vec<&str> = pinned.iter().filter_map(|x| x.as_str()).collect();
    for nt in [
        "a2a_message",
        "agent_message",
        "a2a_task",
        "coordination_request",
    ] {
        assert!(
            pinned.contains(&nt),
            "conversation family must include {nt}"
        );
    }
}

#[tokio::test]
async fn tool_conversation_search_rejects_blank_query() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let err = server
        .call_tool_cli("conversation_search", json!({"query": "   "}))
        .await
        .expect_err("blank query must fail before embedding");
    assert!(format!("{err}").contains("query must be non-empty"));
}

#[tokio::test]
async fn tool_search_mandates_semantic_and_hybrid_modes_execute() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    for mode in ["semantic", "hybrid", "fts"] {
        let result = server
            .call_tool_cli(
                "search_mandates",
                json!({"query": "always run the verification gate", "mode": mode}),
            )
            .await
            .unwrap_or_else(|e| panic!("search_mandates mode={mode} must not error: {e}"));
        let v: serde_json::Value = serde_json::from_str(&text_of(&result))
            .unwrap_or_else(|_| panic!("search_mandates mode={mode} body must be JSON"));
        assert_eq!(v["mode"].as_str(), Some(mode), "echoed mode must match");
        assert!(
            v["results"].is_array(),
            "results must be an array (mode={mode})"
        );
    }

    let err = server
        .call_tool_cli(
            "search_mandates",
            json!({"query": "x", "mode": "not_a_mode"}),
        )
        .await
        .expect_err("invalid mode must be rejected");
    assert!(format!("{err}").contains("mode must be"));
}
