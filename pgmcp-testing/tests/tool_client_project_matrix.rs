//! Real-Postgres integration test for the `client_project_matrix` MCP tool.
//!
//! Seeds `client_file_events` (edits + a read by one Claude Code session) and
//! asserts the tool returns the edit-weighted m:n matrix grouped by project,
//! plus the per-project recently-edited files. Also satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate via the literal
//! `call_tool_cli("client_project_matrix", …)`.

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
async fn client_project_matrix_aggregates_edits_by_project() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "cpm-proj", "/ws/cpm-proj").await;

    // One Claude Code session (hook source): two writes/edits + one read,
    // across two distinct files (a.rs edited, b.rs written, a.rs re-read).
    sqlx::query(
        "INSERT INTO client_file_events (session_id, project_id, abs_path, op, source, ts)
         VALUES
            ('22222222-2222-2222-2222-222222222222', $1, '/ws/cpm-proj/a.rs', 'edit',  'client_hook', now()),
            ('22222222-2222-2222-2222-222222222222', $1, '/ws/cpm-proj/b.rs', 'write', 'client_hook', now()),
            ('22222222-2222-2222-2222-222222222222', $1, '/ws/cpm-proj/a.rs', 'read',  'client_hook', now())",
    )
    .bind(project)
    .execute(&pool)
    .await
    .expect("seed client_file_events");

    let server = server_with_pool(pool);
    let res = server
        .call_tool_cli(
            "client_project_matrix",
            serde_json::json!({ "since_minutes": 60 }),
        )
        .await
        .expect("client_project_matrix ok");
    let json = tool_json(&res);

    assert_eq!(json["project_count"], 1);
    let groups = json["by_project"].as_array().expect("by_project array");
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!(g["project"], "cpm-proj");

    let clients = g["clients"].as_array().expect("clients array");
    assert_eq!(clients.len(), 1, "one session → one client row");
    let c = &clients[0];
    assert_eq!(c["client_name"], "claude-code", "hook source ⇒ claude-code");
    assert_eq!(c["edit_count"], 2, "two writes/edits");
    assert_eq!(c["read_count"], 1, "one read");
    assert_eq!(c["file_count"], 2, "two distinct files");

    let files = g["recent_files"].as_array().expect("recent_files array");
    assert!(
        files.iter().any(|f| f["path"] == "/ws/cpm-proj/a.rs"),
        "a.rs listed: {files:?}"
    );
    assert!(
        files.iter().any(|f| f["path"] == "/ws/cpm-proj/b.rs"),
        "b.rs listed: {files:?}"
    );
}
