//! Integration test for `cross_project_topic_redundancy` (ADR-029, item 14):
//! surfaces global topics spanning ≥min_projects projects. Drives the dispatched
//! tool via `call_tool_cli` (Layer-D coverage gate).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn cross_project_topic_redundancy_surfaces_shared_topics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // A shared global topic (3 projects) and a single-project one.
    sqlx::query(
        "INSERT INTO code_topics
            (scope, cluster_index, label, chunk_count, file_count, project_count, project_names)
         VALUES ('global', 9001, 'xproj-shared-auth', 50, 10, 3, ARRAY['pa','pb','pc']),
                ('global', 9002, 'xproj-solo-thing', 12, 3, 1, ARRAY['pa'])
         ON CONFLICT (scope, cluster_index) DO UPDATE SET project_count = EXCLUDED.project_count",
    )
    .execute(&pool)
    .await
    .expect("seed code_topics");

    let res = body(
        &server
            .call_tool_cli("cross_project_topic_redundancy", json!({"min_projects": 2}))
            .await
            .expect("cross_project_topic_redundancy"),
    );
    let shared = res["shared_topics"].as_array().unwrap();
    let labels: Vec<&str> = shared
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"xproj-shared-auth"),
        "shared topic must appear: {labels:?}"
    );
    assert!(
        !labels.contains(&"xproj-solo-thing"),
        "single-project topic must be excluded at min_projects=2: {labels:?}"
    );
    let auth = shared
        .iter()
        .find(|t| t["label"] == "xproj-shared-auth")
        .unwrap();
    assert_eq!(auth["project_count"].as_i64(), Some(3));
    assert_eq!(auth["projects"].as_array().unwrap().len(), 3);
}
