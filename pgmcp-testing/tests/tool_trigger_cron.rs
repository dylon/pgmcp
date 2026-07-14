//! Integration test for the `trigger_cron` MCP tool.
//!
//! The tool dispatches to three heavy crons by name. We can't easily
//! exercise the full cron bodies in a unit test (each writes substantial
//! state and depends on file scanning), but the dispatch surface itself
//! is what the `every_dispatched_tool_has_an_integration_test` coverage
//! gate cares about: that the call resolves through `call_tool_cli`
//! and returns a structured response. We exercise the invalid-job
//! path (which short-circuits before touching the DB) so the test runs
//! quickly and deterministically.

use std::sync::Arc;

use crate::common::text_of;
use pgmcp::mcp::server::McpServer;
use pgmcp_testing::pool_tool_helpers::{context_with_pool, server_with_pool};
use pgmcp_testing::require_test_db;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn trigger_cron_unknown_job_returns_invalid_params() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // Unknown job names short-circuit with invalid_params before
    // touching any cron body. This validates the dispatch surface.
    let r = server
        .call_tool_cli(
            "trigger_cron",
            serde_json::json!({"job": "this-is-not-a-real-job"}),
        )
        .await;

    // Either the call returns Err (MCP-level), or it returns Ok with
    // is_error=true (tool-level). Both are valid "rejection" shapes.
    match r {
        Ok(result) => {
            assert_eq!(
                result.is_error,
                Some(true),
                "unknown job must produce a tool-level error"
            );
        }
        Err(_) => {
            // McpError::invalid_params propagated up; acceptable.
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_cron_busy_when_heavy_cron_lock_is_held() {
    let db = require_test_db!();
    let ctx = context_with_pool(db.pool().clone());
    let lock = Arc::clone(ctx.heavy_cron_lock());
    let _guard = lock.try_lock().expect("test holds heavy-cron lock");
    let server = McpServer::new(ctx);

    let result = server
        .call_tool_cli(
            "trigger_cron",
            serde_json::json!({"job": " graph-analysis ", "project": "   "}),
        )
        .await
        .expect("busy response");

    assert!(result.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&result)).expect("trigger_cron busy JSON");
    assert_eq!(v["job"].as_str(), Some("graph-analysis"));
    assert_eq!(v["project"], Value::Null);
    assert_eq!(v["status"].as_str(), Some("busy"));
    assert_eq!(v["retry_after_secs"].as_u64(), Some(60));
}
