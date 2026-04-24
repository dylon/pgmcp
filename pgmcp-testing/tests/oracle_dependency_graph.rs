//! Real-Postgres correctness oracle for `dependency_graph`.
//! See `pgmcp-testing/src/fixtures/synthetic_corpus.rs` for the
//! 5-file / 6-import-edge graph the assertions are derived from.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn dependency_graph_summary_reports_correct_node_and_edge_counts() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    // 5 nodes (a, b, c, util, api), 6 import edges.
    assert_eq!(v["node_count"], 5);
    assert_eq!(v["edge_count"], 6);
    // 6 edges form a single connected component (with both cycles).
    assert_eq!(v["components"], 1);
    let counts = v["edge_type_counts"].as_object().expect("counts");
    assert_eq!(counts["import"].as_u64(), Some(6));
}
