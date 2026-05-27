//! Coverage smoke tests (Layer A) for the unified-graph / ontology MCP tools
//! `graph_neighbors` and `recognize_trajectory`. These close the
//! `query_inventory_vs_coverage::every_dispatched_tool_has_an_integration_test`
//! gap: every tool dispatched in `call_tool_cli` must have an integration test
//! that invokes it.
//!
//! Each test calls the tool with minimal valid arguments and asserts `Ok` — the
//! goal is to catch SQL/schema drift (the unified-graph matviews, the trajectory
//! cohort queries), not algorithmic correctness (the `oracle_*.rs` files cover
//! that). Both tools return an empty-but-`Ok` envelope on an unseeded graph, so
//! no fixture seeding is required; like the other DB smoke tests they self-skip
//! when no test database is available.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

/// `graph_neighbors`: a `<type>:<numeric-id>` node-ref resolves via the fast
/// path in `resolve_graph_node_id` (no row lookup needed), then
/// `memory_neighbors` traverses the unified-graph edge matview. On an empty
/// graph the node has no neighbors, so the call returns a well-formed `Ok`
/// envelope — enough to exercise the recursive matview SQL for drift.
#[tokio::test]
async fn tool_graph_neighbors_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli("graph_neighbors", json!({"node_ref": "project:1"}))
        .await
        .expect("graph_neighbors must not error on an empty unified graph");

    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("graph_neighbors body must be JSON");
    assert_eq!(v["resolved_node_id"].as_str(), Some("project:1"));
    assert!(
        !v["neighbors"].is_null(),
        "neighbors envelope must be present"
    );
}

/// `recognize_trajectory`: MSM-matches a partial numeric series against stored
/// `work_item` progress trajectories. With none stored, the ranked set is empty
/// and the call returns `Ok` — enough to exercise the cohort-load SQL + MSM
/// encoding path for drift.
#[tokio::test]
async fn tool_recognize_trajectory_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "recognize_trajectory",
            json!({"node_type": "work_item", "series": [0.1, 0.4, 0.9]}),
        )
        .await
        .expect("recognize_trajectory must not error with no stored trajectories");

    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("recognize_trajectory body must be JSON");
    assert_eq!(v["partial_len"].as_u64(), Some(3));
    assert!(v["nearest"].is_array(), "nearest must be an array");
}
