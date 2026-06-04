//! Real-Postgres integration test for the `active_clients` MCP tool.
//!
//! Seeds `mcp_clients` rows (one alive client on a project, one exited) and
//! asserts the tool envelope groups them by project and honors the
//! `include_exited` + `project` filters. Also satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate via the literal
//! `call_tool_cli("active_clients", …)`.
//!
//! `require_test_db!` skips cleanly when no test DB is configured, so this runs
//! inside `verify.sh` Gate 5 without an `#[ignore]`.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

/// Extract the first text block of a tool result as JSON.
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
async fn active_clients_groups_by_project_and_filters() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "active-clients-proj", "/ws/active-clients-proj").await;

    // One alive client on the project + one exited (different session).
    sqlx::query(
        "INSERT INTO mcp_clients
            (mcp_session_id, client_name, client_version, protocol_version,
             pid, proc_start_ticks, cwd, project_id,
             first_seen, last_seen, last_liveness_at, alive, exited_at)
         VALUES
            ('sess-alive', 'claude-code', '1.0', '2024-11-05', 4242, 100,
             '/ws/active-clients-proj/src', $1, now(), now(), now(), TRUE, NULL),
            ('sess-dead', 'codex-mcp-client', '0.9', '2024-11-05', 4343, 200,
             '/ws/active-clients-proj', $1, now(), now() - interval '1 hour',
             now() - interval '1 hour', FALSE, now() - interval '30 min')",
    )
    .bind(project)
    .execute(&pool)
    .await
    .expect("seed mcp_clients");

    let server = server_with_pool(pool);

    // Default (alive only): one client, grouped under the project.
    let res = server
        .call_tool_cli("active_clients", serde_json::json!({}))
        .await
        .expect("active_clients ok");
    let json = tool_json(&res);
    assert_eq!(json["total"], 1, "only the alive client by default");
    let groups = json["by_project"].as_array().expect("by_project array");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["project"], "active-clients-proj");
    assert_eq!(groups[0]["clients"][0]["client_name"], "claude-code");
    assert_eq!(groups[0]["clients"][0]["pid"], 4242);
    assert_eq!(groups[0]["clients"][0]["alive"], true);

    // include_exited: both clients surface.
    let res = server
        .call_tool_cli(
            "active_clients",
            serde_json::json!({ "include_exited": true }),
        )
        .await
        .expect("active_clients include_exited ok");
    let json = tool_json(&res);
    assert_eq!(json["total"], 2, "alive + exited with include_exited");

    // project filter (non-matching) → zero.
    let res = server
        .call_tool_cli(
            "active_clients",
            serde_json::json!({ "project": "nonexistent" }),
        )
        .await
        .expect("active_clients filtered ok");
    let json = tool_json(&res);
    assert_eq!(json["total"], 0);
}
