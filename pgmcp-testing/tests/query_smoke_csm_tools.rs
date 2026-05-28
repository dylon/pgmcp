//! Layer A smoke tests for the CSM/MPST coordination observer tools (ADR-009).
//!
//! Each test calls a `csm_*` tool via `McpServer::call_tool_cli` against a
//! fully-migrated (v8) test DB and asserts it executes without a SQL/schema
//! error — the orient-class regression Layer A guards against. They also satisfy
//! the Layer-D coverage net (`query_inventory_vs_coverage.rs`), which requires
//! every dispatched tool to have a `call_tool_cli("<name>", …)` invocation.
//!
//! The registry/projection tools (`list_protocols`, `protocol_of_pattern`,
//! `show_projection`, `protocol_plan`) are pure over the in-memory protocol
//! registry; `infer_peer_fsm` returns a graceful empty result below
//! `min_support`; `validate_run` against a non-existent task returns a typed
//! "not found" (proving its SQL ran), which we tolerate distinctly from a schema
//! error.

mod common;

use common::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::json;

#[tokio::test]
async fn tool_csm_list_protocols_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let _ = server
        .call_tool_cli("csm_list_protocols", json!({}))
        .await
        .expect("csm_list_protocols must not error");
}

#[tokio::test]
async fn tool_csm_protocol_of_pattern_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let _ = server
        .call_tool_cli(
            "csm_protocol_of_pattern",
            json!({"pattern": "deliberation"}),
        )
        .await
        .expect("csm_protocol_of_pattern must not error");
}

#[tokio::test]
async fn tool_csm_show_projection_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let _ = server
        .call_tool_cli("csm_show_projection", json!({"protocol": "deliberation"}))
        .await
        .expect("csm_show_projection must not error");
}

#[tokio::test]
async fn tool_csm_protocol_plan_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let _ = server
        .call_tool_cli("csm_protocol_plan", json!({"pattern": "sequential"}))
        .await
        .expect("csm_protocol_plan must not error");
}

#[tokio::test]
async fn tool_csm_infer_peer_fsm_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // Empty trace store ⇒ below `min_support` ⇒ a graceful Ok envelope.
    let _ = server
        .call_tool_cli("csm_infer_peer_fsm", json!({"protocol": "deliberation"}))
        .await
        .expect("csm_infer_peer_fsm must not error on an empty trace store");
}

#[tokio::test]
async fn tool_csm_validate_run_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // A well-formed but non-existent task UUID: `read_run`'s SQL must execute
    // cleanly and the tool must fail with a typed "not found" — NOT a SQL/schema
    // error (the orient-class regression this layer guards against).
    let res = server
        .call_tool_cli(
            "csm_validate_run",
            json!({"task_id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    if let Err(e) = res {
        let msg = format!("{e}");
        assert!(
            msg.contains("not found"),
            "csm_validate_run on a missing task must fail with a typed 'not found', \
             not a SQL/schema error: {msg}"
        );
    }
}
