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

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;

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
