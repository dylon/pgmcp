//! Integration tests for the agent-feedback + voting tool family (ADR-023, v43).
//!
//! Exercises every dispatched tool end-to-end through `McpServer::call_tool_cli`
//! against a real `TestDatabase` — which the Layer-D coverage gate
//! (`query_inventory_vs_coverage.rs`) also requires. The test server wires a
//! deterministic 1024-d embedding backend, so feedback embed-on-write + the
//! semantic `search_feedback` leg run for real. Under CLI dispatch the MCP
//! caller identity is absent, so `agent_id` defaults to "unknown-agent" — both
//! votes below share it, which is exactly what exercises the one-vote-per-agent
//! upsert.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(result)).expect("tool body must be JSON")
}

#[tokio::test]
async fn feedback_full_lifecycle() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // submit (validates category/sentiment vocab + embed-on-write).
    let submitted = body(
        &server
            .call_tool_cli(
                "submit_feedback",
                json!({
                    "category": "feature_request",
                    "sentiment": "positive",
                    "subject": "wish: zztop scoping filter",
                    "body": "Please add a zztop_unique_token lexical scope filter to search.",
                    "about_tool": "semantic_search"
                }),
            )
            .await
            .expect("submit_feedback must not error"),
    );
    assert_eq!(submitted["category"], "feature_request");
    assert_eq!(submitted["status"], "open");
    let fid = submitted["id"].as_i64().expect("feedback id");

    // invalid vocab must fail closed.
    assert!(
        server
            .call_tool_cli(
                "submit_feedback",
                json!({"category": "nope", "sentiment": "positive", "body": "x"})
            )
            .await
            .is_err(),
        "an invalid category must be rejected"
    );

    // list (filtered) finds it.
    let listed = body(
        &server
            .call_tool_cli("list_feedback", json!({"category": "feature_request"}))
            .await
            .expect("list_feedback"),
    );
    assert!(
        listed["feedback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"].as_i64() == Some(fid)),
        "submitted feedback must appear in the filtered list"
    );

    // search (hybrid) finds it via the unique token (embed-on-write ran).
    let found = body(
        &server
            .call_tool_cli(
                "search_feedback",
                json!({"query": "zztop_unique_token scope filter", "mode": "hybrid"}),
            )
            .await
            .expect("search_feedback"),
    );
    assert!(
        found["feedback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"].as_i64() == Some(fid)),
        "search must surface the submitted feedback"
    );

    // vote on the feedback; re-voting (same agent) is an idempotent update.
    let t1 = body(
        &server
            .call_tool_cli(
                "cast_vote",
                json!({"target_type": "feedback", "target_id": fid, "direction": "up"}),
            )
            .await
            .expect("cast_vote up"),
    );
    assert_eq!(t1["tally"]["up_votes"].as_i64(), Some(1));
    assert_eq!(t1["tally"]["voters"].as_i64(), Some(1));

    let t2 = body(
        &server
            .call_tool_cli(
                "cast_vote",
                json!({"target_type": "feedback", "target_id": fid, "direction": "down"}),
            )
            .await
            .expect("cast_vote down (same agent → update)"),
    );
    assert_eq!(
        t2["tally"]["voters"].as_i64(),
        Some(1),
        "one vote per agent"
    );
    assert_eq!(t2["tally"]["down_votes"].as_i64(), Some(1));
    assert_eq!(t2["tally"]["up_votes"].as_i64(), Some(0));

    // tally directly.
    let tally = body(
        &server
            .call_tool_cli(
                "tally_votes",
                json!({"target_type": "feedback", "target_id": fid}),
            )
            .await
            .expect("tally_votes"),
    );
    assert_eq!(tally["tally"]["down_votes"].as_i64(), Some(1));

    // retract.
    let retracted = body(
        &server
            .call_tool_cli(
                "retract_vote",
                json!({"target_type": "feedback", "target_id": fid}),
            )
            .await
            .expect("retract_vote"),
    );
    assert_eq!(retracted["removed"], true);
    let after = body(
        &server
            .call_tool_cli(
                "tally_votes",
                json!({"target_type": "feedback", "target_id": fid}),
            )
            .await
            .expect("tally after retract"),
    );
    assert_eq!(after["tally"]["voters"].as_i64(), Some(0));

    // respond (triage).
    let responded = body(
        &server
            .call_tool_cli(
                "respond_feedback",
                json!({"id": fid, "status": "planned", "response": "queued for v-next"}),
            )
            .await
            .expect("respond_feedback"),
    );
    assert_eq!(responded["status"], "planned");
    assert_eq!(responded["updated"], true);

    // promote → work-item (idempotent).
    let promoted = body(
        &server
            .call_tool_cli("promote_feedback_to_work_item", json!({"id": fid}))
            .await
            .expect("promote_feedback_to_work_item"),
    );
    assert_eq!(promoted["already_promoted"], false);
    let wid = promoted["work_item_id"].as_i64().expect("work item id");
    let again = body(
        &server
            .call_tool_cli("promote_feedback_to_work_item", json!({"id": fid}))
            .await
            .expect("promote again (idempotent)"),
    );
    assert_eq!(again["already_promoted"], true);
    assert_eq!(again["work_item_id"].as_i64(), Some(wid));
}
