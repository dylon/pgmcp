//! Integration test for `documented_tech_debt` MCP tool.
//!
//! Seeds a project + a file (no content needed — the tool tolerates absent
//! content) and verifies the tool dispatches without error. The literal
//! `call_tool_cli("documented_tech_debt", …)` satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_summary_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-p", "/ws/dtd-p").await;
    seed_file(db.pool(), p, "/ws/dtd-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": "dtd-p"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_full_format_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-full", "/ws/dtd-full").await;
    seed_file(db.pool(), p, "/ws/dtd-full/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": "dtd-full", "format": "full"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_severity_filter_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-sev", "/ws/dtd-sev").await;
    seed_file(db.pool(), p, "/ws/dtd-sev/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": "dtd-sev", "severity": "high"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
