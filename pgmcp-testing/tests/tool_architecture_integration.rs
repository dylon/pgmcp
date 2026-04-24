//! Phase 6: real-DB integration tests for the Architecture tool category.
//! `coupling_cohesion_report`, `architecture_violations`,
//! `design_smell_detection`, `architecture_quality`, `design_metrics`.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn coupling_cohesion_report_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ccr-p", "/ws/ccr-p").await;
    seed_file(db.pool(), p, "/ws/ccr-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "ccr-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn architecture_violations_runs_with_no_rules() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "av-p", "/ws/av-p").await;
    seed_file(db.pool(), p, "/ws/av-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({"project": "av-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn design_smell_detection_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dsd-p", "/ws/dsd-p").await;
    seed_file(db.pool(), p, "/ws/dsd-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "design_smell_detection",
            serde_json::json!({"project": "dsd-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn architecture_quality_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "aq-p", "/ws/aq-p").await;
    seed_file(db.pool(), p, "/ws/aq-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "aq-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn design_metrics_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dm-p", "/ws/dm-p").await;
    seed_file(db.pool(), p, "/ws/dm-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("design_metrics", serde_json::json!({"project": "dm-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}
