//! Phase 6: real-DB integration tests for the Graph tool category.
//! `dependency_graph`, `centrality_analysis`, `community_detection`,
//! `circular_dependencies`, `change_impact_analysis` — all pool()-using.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn dependency_graph_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "graph-p", "/ws/graph-p").await;
    seed_file(db.pool(), p, "/ws/graph-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn centrality_analysis_empty_graph_returns_graceful() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "central-p", "/ws/central-p").await;
    seed_file(db.pool(), p, "/ws/central-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": "central-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn community_detection_on_empty_project_is_graceful() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "comm-p", "/ws/comm-p").await;
    seed_file(db.pool(), p, "/ws/comm-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "community_detection",
            serde_json::json!({"project": "comm-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn circular_dependencies_empty_graph_finds_none() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "cycle-p", "/ws/cycle-p").await;
    seed_file(db.pool(), p, "/ws/cycle-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "circular_dependencies",
            serde_json::json!({"project": "cycle-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn change_impact_analysis_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "impact-p", "/ws/impact-p").await;
    seed_file(db.pool(), p, "/ws/impact-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "change_impact_analysis",
            serde_json::json!({"project": "impact-p", "files": ["a.rs"]}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}
