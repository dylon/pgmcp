//! Integration test for the `security_scan` MCP tool.
//!
//! Exercises the dispatch surface — the `every_dispatched_tool_has_an_integration_test`
//! coverage gate needs a literal `call_tool_cli("security_scan", …)` here — plus the
//! cached READ path (`refresh=false`) against a real migrated test DB, where
//! `external_scanner_findings` (migration v34) exists but is empty. We deliberately
//! avoid `refresh=true` with a free lock so the test never spawns real scanner
//! subprocesses (slow / environment-dependent); the busy path is checked instead.

use std::sync::Arc;

use crate::common::text_of;
use pgmcp::mcp::server::McpServer;
use pgmcp_testing::pool_tool_helpers::{context_with_pool, server_with_pool};
use pgmcp_testing::require_test_db;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn security_scan_cached_read_returns_structured_shape() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // refresh=false ⇒ no subprocess sweep; reads `external_scanner_findings`.
    let result = server
        .call_tool_cli(
            "security_scan",
            serde_json::json!({ "refresh": false, "severity_min": "high", "limit": 10 }),
        )
        .await
        .expect("security_scan cached read");

    assert!(result.is_error != Some(true), "cached read must not error");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("security_scan JSON");
    assert_eq!(v["refreshed"].as_bool(), Some(false));
    assert!(v["count"].is_number(), "count present");
    assert!(v["findings"].is_array(), "findings is an array");
    assert!(v["by_scanner"].is_object(), "by_scanner summary present");
    assert!(v["by_severity"].is_object(), "by_severity summary present");
}

#[tokio::test(flavor = "multi_thread")]
async fn security_scan_scoped_to_unknown_project_is_empty() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "security_scan",
            serde_json::json!({ "project": "definitely-not-a-real-project-xyz", "refresh": false }),
        )
        .await
        .expect("security_scan scoped read");

    let v: Value = serde_json::from_str(&text_of(&result)).expect("JSON");
    assert_eq!(
        v["count"].as_u64(),
        Some(0),
        "a project filter that matches nothing ⇒ zero findings"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn security_scan_refresh_is_busy_when_heavy_lock_held() {
    let db = require_test_db!();
    let ctx = context_with_pool(db.pool().clone());
    let lock = Arc::clone(ctx.heavy_cron_lock());
    let _guard = lock.try_lock().expect("test holds heavy-cron lock");
    let server = McpServer::new(ctx);

    // refresh=true needs the heavy-cron lock; held ⇒ a "busy" response and NO
    // scanner sweep is attempted.
    let result = server
        .call_tool_cli("security_scan", serde_json::json!({ "refresh": true }))
        .await
        .expect("busy response");

    assert!(result.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&result)).expect("busy JSON");
    assert_eq!(v["status"].as_str(), Some("busy"));
    assert_eq!(v["retry_after_secs"].as_u64(), Some(60));
}
