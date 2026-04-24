//! Phase 6: real-DB integration tests for the Prediction tool category.
//! `bug_prediction`, `technical_debt_analysis`, `anomaly_detection`.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn bug_prediction_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "bp-p", "/ws/bp-p").await;
    seed_file(db.pool(), p, "/ws/bp-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("bug_prediction", serde_json::json!({"project": "bp-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn technical_debt_analysis_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "tdebt-p", "/ws/tdebt-p").await;
    seed_file(db.pool(), p, "/ws/tdebt-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "technical_debt_analysis",
            serde_json::json!({"project": "tdebt-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn anomaly_detection_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "anom-p", "/ws/anom-p").await;
    seed_file(db.pool(), p, "/ws/anom-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "anomaly_detection",
            serde_json::json!({"project": "anom-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}
