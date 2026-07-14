//! Integration test for topic ⊗ experiment map (#7, ADR-029): experiments
//! anchored to a topic surface under that topic. Coverage gate via call_tool_cli.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn topic_experiment_map_links_experiments_to_topics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let topic_id: i32 = sqlx::query_scalar(
        "INSERT INTO code_topics (scope, cluster_index, label, chunk_count, file_count, project_count, project_names)
         VALUES ('global', 8800, 'te-retrieval', 5, 2, 1, ARRAY['p'])
         ON CONFLICT (scope, cluster_index) DO UPDATE SET label='te-retrieval' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("topic");
    let exp_id: i64 = sqlx::query_scalar(
        "INSERT INTO experiments (title) VALUES ('improve retrieval recall') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("experiment");
    sqlx::query("INSERT INTO experiment_code_anchor (experiment_id, topic_id) VALUES ($1, $2)")
        .bind(exp_id)
        .bind(topic_id)
        .execute(&pool)
        .await
        .expect("anchor");

    let res = body(
        &server
            .call_tool_cli("topic_experiment_map", json!({}))
            .await
            .expect("topic_experiment_map"),
    );
    let topics = res["topics"].as_array().expect("topics");
    let t = topics
        .iter()
        .find(|t| t["topic_id"].as_i64() == Some(topic_id as i64))
        .expect("our topic present");
    assert_eq!(t["experiment_count"].as_i64(), Some(1), "{t}");
    assert!(
        t["experiments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "improve retrieval recall"),
        "{t}"
    );
}
