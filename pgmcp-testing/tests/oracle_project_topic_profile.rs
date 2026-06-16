//! Real-Postgres oracle for `project_topic_profile`.
//!
//! On the synthetic corpus, `proj-auth` is dominated by the planted "auth"
//! topic (10 auth chunks vs. a handful of split-candidate chunks), so its
//! specialization index is high and its top topic is "auth".

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
async fn project_topic_profile_fingerprints_a_focused_project() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "project_topic_profile",
            serde_json::json!({"project": "proj-auth"}),
        )
        .await
        .expect("project_topic_profile call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let p = &v["projects"][0];
    assert_eq!(p["project"], "proj-auth", "profile is for proj-auth: {v}");
    assert_eq!(
        p["top_topics"][0]["label"], "auth",
        "auth is the dominant topic: {v}"
    );
    assert!(
        p["specialization_index"].as_f64().expect("spec index") > 0.5,
        "an auth-dominated project should read as specialized: {v}"
    );

    // Unknown format fails closed (parsed before any DB work).
    let err = server
        .call_tool_cli(
            "project_topic_profile",
            serde_json::json!({"format": "bogus"}),
        )
        .await
        .expect_err("unknown format must fail");
    assert!(
        err.to_string().contains("unknown format"),
        "unexpected format error: {err}"
    );
}

#[tokio::test]
async fn project_topic_profile_all_projects_returns_comparison() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("project_topic_profile", serde_json::json!({}))
        .await
        .expect("all-projects profile");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let projects = v["projects"].as_array().expect("projects array");
    // All three seeded projects have topic assignments.
    assert!(
        projects.len() >= 3,
        "expected ≥3 profiled projects, got {}: {v}",
        projects.len()
    );
    assert!(
        projects.iter().any(|p| p["project"] == "proj-database"),
        "proj-database should be profiled: {v}"
    );
}
