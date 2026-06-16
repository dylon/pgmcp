//! Real-Postgres oracle for `project_topic_similarity`.
//!
//! The synthetic corpus' three projects cluster around orthonormal embedding
//! bases (auth≈e0, database≈e1, logging≈e2), so they are mutually distinct: the
//! tool must report 3 projects, find NO redundant forks, and keep cross-project
//! similarity below the clustering threshold. (Fork-detection itself is covered
//! deterministically by the `similarity` unit tests.)

mod common;

use common::server_with_pool;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;

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

#[tokio::test]
async fn project_topic_similarity_separates_distinct_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("project_topic_similarity", serde_json::json!({}))
        .await
        .expect("project_topic_similarity call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["method"], "centroid");
    assert_eq!(v["n_projects"], 3, "three seeded projects: {v}");
    assert_eq!(
        v["redundant_forks"].as_array().map(|a| a.len()),
        Some(0),
        "distinct orthogonal projects must not be flagged as forks: {v}"
    );
    // The most-similar cross pair must sit below the default 0.85 threshold.
    let pairs = v["pairwise_top"].as_array().expect("pairwise array");
    assert!(!pairs.is_empty(), "expected pairwise similarities: {v}");
    let top = pairs[0]["sim"].as_f64().expect("sim");
    assert!(
        top < 0.85,
        "orthogonal projects should be dissimilar, got top sim {top}: {v}"
    );

    // global_jsd is a valid alternate lens; an unknown method fails closed.
    server
        .call_tool_cli(
            "project_topic_similarity",
            serde_json::json!({"method": "global_jsd"}),
        )
        .await
        .expect("global_jsd method runs");
    let err = server
        .call_tool_cli(
            "project_topic_similarity",
            serde_json::json!({"method": "nonsense"}),
        )
        .await
        .expect_err("unknown method must fail");
    assert!(
        err.to_string().contains("method must be"),
        "unexpected method error: {err}"
    );
}
