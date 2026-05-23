//! `a2a_cancel_task` — cancel a Task on a registered A2A peer.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;
use uuid::Uuid;

use crate::a2a::client::A2aClient;
use crate::context::SystemContext;
use crate::mcp::server::A2aCancelTaskParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_cancel_task(
    ctx: &SystemContext,
    params: A2aCancelTaskParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_cancel_task", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let row: Option<(String,)> =
        sqlx::query_as::<_, (String,)>("SELECT url FROM a2a_agents WHERE name = $1")
            .bind(&params.target_agent)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Agent lookup failed: {}", e), None))?;
    let url = row.map(|(u,)| u).ok_or_else(|| {
        McpError::internal_error(
            format!("Agent not registered: {}", params.target_agent),
            None,
        )
    })?;

    let task_id = Uuid::parse_str(&params.task_id)
        .map_err(|e| McpError::internal_error(format!("Bad task_id: {}", e), None))?;
    let client = A2aClient::new(url);
    let task = client
        .cancel_task(task_id)
        .await
        .map_err(|e| McpError::internal_error(format!("A2A cancel failed: {}", e), None))?;
    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,"target_agent": params.target_agent, "task": task}))
}
