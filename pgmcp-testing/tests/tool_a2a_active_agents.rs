//! Real-Postgres integration test for the `a2a_active_agents` MCP tool — the
//! A2A active-agents-by-project discovery view. Also satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate via the literal
//! `call_tool_cli("a2a_active_agents", …)`.

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
async fn a2a_active_agents_groups_by_project() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "a2a-aa-proj", "/ws/a2a-aa-proj").await;
    sqlx::query(
        "INSERT INTO mcp_clients
            (mcp_session_id, client_name, pid, cwd, project_id, first_seen, last_seen, alive)
         VALUES ('aa-sess', 'claude-code', 7777, '/ws/a2a-aa-proj/src', $1, now(), now(), TRUE)",
    )
    .bind(project)
    .execute(&pool)
    .await
    .expect("seed mcp_clients");

    let server = server_with_pool(pool);
    let res = server
        .call_tool_cli("a2a_active_agents", serde_json::json!({}))
        .await
        .expect("a2a_active_agents ok");
    let json = tool_json(&res);

    assert_eq!(json["total"], 1);
    let groups = json["by_project"].as_array().expect("by_project array");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["project"], "a2a-aa-proj");
    let agents = groups[0]["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["client_name"], "claude-code");
    assert_eq!(agents[0]["mcp_session_id"], "aa-sess");
    assert_eq!(agents[0]["pid"], 7777);
}
