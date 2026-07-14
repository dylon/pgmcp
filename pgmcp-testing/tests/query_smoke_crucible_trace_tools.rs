//! Layer A + Layer D smoke tests for the `crucible_trace_*` run-tracing tools
//! (ADR-020 E10). Each of the 15 dispatched tools is exercised via
//! `McpServer::call_tool_cli` against a fully-migrated test DB, asserting it runs
//! without a SQL/schema error — the orient-class regression Layer A guards. The
//! test builds a REAL trace (`open_span` → a span_id) and threads it through the
//! record/query tools so they return `Ok` on populated input; the two replay
//! tools (`replay`/`reconcile`) resolve a linked orchestration session's
//! `global_type`, which our smoke trace has none of, so their typed
//! "no session linked" error is tolerated (the SQL still ran). This file also
//! satisfies the Layer-D coverage net (`query_inventory_vs_coverage.rs`), which
//! requires every dispatched tool to have a `call_tool_cli("<name>", …)`
//! invocation.

use crate::common::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::json;

/// Extract the first text block from a tool result (every crucible_trace tool
/// emits one JSON text block via `json_result`).
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

/// A smoke test tolerates a *typed* business error (invalid_params / not-found):
/// it proves the tool's SQL executed and it reached its logic. A SQL/schema
/// error (a missing column/relation, a syntax error, a constraint violation) is
/// the orient-class regression this layer exists to catch — never tolerated.
fn assert_ok_or_typed<E: std::fmt::Display>(
    res: Result<rmcp::model::CallToolResult, E>,
    tool: &str,
) {
    if let Err(e) = res {
        let msg = format!("{e}").to_lowercase();
        let schema_smell = [
            "column",
            "relation \"",
            "syntax error",
            "does not exist",
            "violates",
            "no such column",
            "undefined table",
        ]
        .iter()
        .any(|m| msg.contains(m));
        assert!(
            !schema_smell,
            "{tool} returned what looks like a SQL/schema error (not a typed \
             business error): {e}"
        );
    }
}

#[tokio::test]
async fn crucible_trace_tools_end_to_end_smoke() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let trace_id = uuid::Uuid::new_v4().to_string();
    let other_trace = uuid::Uuid::new_v4().to_string();

    // open_span → a real run-root span; capture its span_id for the threaded calls.
    let opened = server
        .call_tool_cli(
            "crucible_trace_open_span",
            json!({ "trace_id": trace_id, "kind": "run", "name": "smoke: run root" }),
        )
        .await
        .expect("crucible_trace_open_span must not error");
    let opened_json: serde_json::Value =
        serde_json::from_str(&text_of(&opened)).expect("open_span returns JSON");
    let span_id = opened_json["span_id"]
        .as_i64()
        .expect("open_span returns a span_id");

    // record_span — one-shot child step span under the root.
    server
        .call_tool_cli(
            "crucible_trace_record_span",
            json!({
                "trace_id": trace_id, "kind": "planned_step", "name": "smoke: step 1",
                "parent_span_id": span_id, "status": "ok",
            }),
        )
        .await
        .expect("crucible_trace_record_span must not error");

    // event — append a point-in-time annotation to the open span.
    server
        .call_tool_cli(
            "crucible_trace_event",
            json!({
                "span_id": span_id, "trace_id": trace_id, "event_kind": "model_chosen",
                "severity": "info", "message": "smoke",
            }),
        )
        .await
        .expect("crucible_trace_event must not error");

    // record_counterexample — persist a witness on this trace.
    server
        .call_tool_cli(
            "crucible_trace_record_counterexample",
            json!({
                "source": "tlc", "witness_kind": "event_trace", "witness": { "steps": [] },
                "content_sha256": "a".repeat(64), "trace_id": trace_id, "verdict": "violated",
                "property": "smoke_invariant",
            }),
        )
        .await
        .expect("crucible_trace_record_counterexample must not error");

    // control — append a control-plane action to the audit journal.
    server
        .call_tool_cli(
            "crucible_trace_control",
            json!({ "action": "checkpoint", "scope": "fleet", "reason": "smoke", "actor": "mcp" }),
        )
        .await
        .expect("crucible_trace_control must not error");

    // close_span — terminal status + ended_at on the open span.
    server
        .call_tool_cli(
            "crucible_trace_close_span",
            json!({ "span_id": span_id, "status": "ok" }),
        )
        .await
        .expect("crucible_trace_close_span must not error");

    // get / timeline / why — the trace now has spans, so these return Ok.
    server
        .call_tool_cli("crucible_trace_get", json!({ "trace_id": trace_id }))
        .await
        .expect("crucible_trace_get must not error");
    server
        .call_tool_cli("crucible_trace_timeline", json!({ "trace_id": trace_id }))
        .await
        .expect("crucible_trace_timeline must not error");
    server
        .call_tool_cli("crucible_trace_why", json!({ "trace_id": trace_id }))
        .await
        .expect("crucible_trace_why must not error");

    // query / audit — cross-trace reads; empty filters are valid.
    server
        .call_tool_cli("crucible_trace_query", json!({ "limit": 10 }))
        .await
        .expect("crucible_trace_query must not error");
    server
        .call_tool_cli("crucible_trace_audit", json!({ "limit": 10 }))
        .await
        .expect("crucible_trace_audit must not error");

    // diff — structural diff of two traces (the second loads empty; still Ok).
    server
        .call_tool_cli(
            "crucible_trace_diff",
            json!({ "failing": trace_id, "passing": other_trace }),
        )
        .await
        .expect("crucible_trace_diff must not error");

    // counterexample — fetch the latest witness for this trace (tolerate a typed
    // "no matching counterexample" should the fetch-by-trace path differ).
    assert_ok_or_typed(
        server
            .call_tool_cli(
                "crucible_trace_counterexample",
                json!({ "trace_id": trace_id }),
            )
            .await,
        "crucible_trace_counterexample",
    );

    // replay / reconcile — resolve the run's linked orchestration session
    // (global_type). Our smoke trace has none, so a typed "no session linked"
    // error is expected and tolerated; a SQL/schema error would still fail.
    assert_ok_or_typed(
        server
            .call_tool_cli("crucible_trace_replay", json!({ "trace_id": trace_id }))
            .await,
        "crucible_trace_replay",
    );
    assert_ok_or_typed(
        server
            .call_tool_cli("crucible_trace_reconcile", json!({ "trace_id": trace_id }))
            .await,
        "crucible_trace_reconcile",
    );
}
