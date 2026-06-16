//! Real-Postgres oracle for `topic_coverage_gaps`.
//!
//! The synthetic corpus plants exactly one orphan chunk in `proj-auth`, and its
//! database/logging topics carry a single chunk each (below the default thin
//! threshold), so the report must surface 1 orphan and ≥2 thin topics.

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
async fn topic_coverage_gaps_reports_orphans_and_thin_topics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "topic_coverage_gaps",
            serde_json::json!({"project": "proj-auth"}),
        )
        .await
        .expect("topic_coverage_gaps call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let p = &v["projects"][0];
    assert_eq!(p["project"], "proj-auth");
    assert_eq!(
        p["orphan_chunk_count"].as_i64(),
        Some(1),
        "proj-auth has exactly one planted orphan chunk: {v}"
    );
    // database + logging each have a single split-candidate chunk → thin.
    let thin = p["thin_topics"].as_array().expect("thin_topics");
    assert!(
        thin.len() >= 2,
        "expected ≥2 thin topics in proj-auth, got {}: {v}",
        thin.len()
    );
}
