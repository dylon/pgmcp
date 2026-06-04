//! Real-Postgres integration test for the A2A mailbox tools — the full
//! lifecycle: `a2a_send_message` → `a2a_inbox` → `a2a_reply_message` →
//! `a2a_ack_message`. Satisfies the `every_dispatched_tool_has_an_integration_test`
//! coverage gate for all four via the literal `call_tool_cli("…")`.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    serde_json::from_str(&text).expect("tool output is JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_mailbox_send_inbox_reply_ack_lifecycle() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _project = seed_project(&pool, "mbx-proj", "/ws/mbx-proj").await;
    let server = server_with_pool(pool);

    // A (session sess-A) sends a request to B's instance (sess-B).
    let res = server
        .call_tool_cli(
            "a2a_send_message",
            serde_json::json!({
                "to_session": "sess-B",
                "kind": "request",
                "subject": "hold edits",
                "body": "rebasing core/, please hold",
                "from_agent": "claude-code",
                "from_session": "sess-A"
            }),
        )
        .await
        .expect("send ok");
    let msg_id = tool_json(&res)["message_id"].as_i64().expect("message_id");

    // B reads its inbox → sees the message; reading marks it read for sess-B.
    let res = server
        .call_tool_cli("a2a_inbox", serde_json::json!({ "session": "sess-B" }))
        .await
        .expect("inbox B ok");
    let inbox = tool_json(&res);
    assert_eq!(inbox["count"], 1);
    assert_eq!(inbox["messages"][0]["id"], msg_id);
    assert_eq!(inbox["messages"][0]["from_agent"], "claude-code");
    assert_eq!(inbox["messages"][0]["kind"], "request");

    // B replies → the reply is addressed back to A (sess-A).
    let res = server
        .call_tool_cli(
            "a2a_reply_message",
            serde_json::json!({
                "message_id": msg_id,
                "body": "ok, holding",
                "from_agent": "claude-code",
                "from_session": "sess-B"
            }),
        )
        .await
        .expect("reply ok");
    let reply = tool_json(&res);
    assert_eq!(reply["in_reply_to"], msg_id);
    assert_eq!(reply["to_session"], "sess-A");

    // A reads its inbox → sees the reply.
    let res = server
        .call_tool_cli("a2a_inbox", serde_json::json!({ "session": "sess-A" }))
        .await
        .expect("inbox A ok");
    let inbox_a = tool_json(&res);
    assert_eq!(inbox_a["count"], 1);
    assert_eq!(inbox_a["messages"][0]["body"], "ok, holding");

    // B acks the original message.
    let res = server
        .call_tool_cli(
            "a2a_ack_message",
            serde_json::json!({ "message_id": msg_id, "session": "sess-B" }),
        )
        .await
        .expect("ack ok");
    assert_eq!(tool_json(&res)["acked"], msg_id);

    // unread_only now hides the read original for B.
    let res = server
        .call_tool_cli(
            "a2a_inbox",
            serde_json::json!({ "session": "sess-B", "unread_only": true }),
        )
        .await
        .expect("inbox B unread ok");
    assert_eq!(
        tool_json(&res)["count"],
        0,
        "read message hidden by unread_only"
    );
}
