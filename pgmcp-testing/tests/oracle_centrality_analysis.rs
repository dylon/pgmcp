//! Real-Postgres correctness oracle for `centrality_analysis`. The
//! synthetic graph corpus pre-loads `file_metrics` rows with known
//! pagerank scores so the rank order is deterministic.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn centrality_analysis_pagerank_orders_files_by_pinned_metrics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": "graph-proj", "metric": "pagerank"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 5);
    // Pinned metrics: a=0.30, b=0.20, util=0.20, c=0.15, api=0.15
    // — top file must be core/a.rs.
    assert_eq!(files[0]["path"].as_str(), Some("core/a.rs"));
    let top_pr: f64 = files[0]["pagerank"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (top_pr - 0.30).abs() < 1e-3,
        "top pagerank = {top_pr}, expected 0.30"
    );
}
