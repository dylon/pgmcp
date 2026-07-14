//! Real-Postgres oracle for `topic_project_map`.
//!
//! The synthetic corpus' global topics carry no `project_names` (only a real
//! global roll-up sets those), so the test stamps a breadth-2 incidence on the
//! "auth" theme and asserts the map surfaces it.

use crate::common::server_with_pool;
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
async fn topic_project_map_surfaces_shared_themes() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    // Stamp a cross-project incidence on the global "auth" theme.
    sqlx::query(
        "UPDATE code_topics
         SET project_names = ARRAY['proj-auth','proj-database']::text[], project_count = 2
         WHERE scope = 'global' AND label = 'auth'",
    )
    .execute(&pool)
    .await
    .expect("stamp incidence");
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("topic_project_map", serde_json::json!({"min_breadth": 2}))
        .await
        .expect("topic_project_map call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let themes = v["themes"].as_array().expect("themes array");
    let auth = themes
        .iter()
        .find(|t| t["label"] == "auth")
        .expect("auth theme present at breadth 2");
    assert_eq!(auth["breadth"], 2, "auth spans two projects: {v}");
    let projects: Vec<&str> = auth["projects"]
        .as_array()
        .expect("projects")
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect();
    assert!(
        projects.contains(&"proj-auth") && projects.contains(&"proj-database"),
        "auth theme lists both contributing projects: {v}"
    );
}

#[tokio::test]
async fn topic_project_map_guides_when_no_global_rollup() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    // Chunks only — no global topics at all.
    SyntheticCorpus::seed_chunks_only(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("topic_project_map", serde_json::json!({}))
        .await
        .expect("topic_project_map call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert!(
        v["guidance"]
            .as_str()
            .map(|s| s.contains("discover_topics"))
            .unwrap_or(false),
        "expected discover_topics guidance when no roll-up: {v}"
    );
}
