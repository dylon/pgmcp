//! Phase 6: real-DB integration tests for the Scorecard tool category.
//! `engineering_scorecard`, `code_summarize`.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn engineering_scorecard_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "score-p", "/ws/score-p").await;
    seed_file(db.pool(), p, "/ws/score-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "engineering_scorecard",
            serde_json::json!({"project": "score-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn code_summarize_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "sum-p", "/ws/sum-p").await;
    seed_file(db.pool(), p, "/ws/sum-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("code_summarize", serde_json::json!({"project": "sum-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}
