//! `a2a_subscribe_task` — return the SSE URL for a peer's Task so the
//! caller can stream events. (Direct streaming inside an MCP tool would
//! require streaming MCP responses; here we surface the URL.)

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::A2aSubscribeTaskParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_subscribe_task(
    ctx: &SystemContext,
    params: A2aSubscribeTaskParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_subscribe_task", "MCP tool invoked");
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

    // Derive SSE URL from JSON-RPC URL.
    let base = url
        .strip_suffix("/a2a/jsonrpc")
        .unwrap_or(url.trim_end_matches('/'));
    let sse_url = format!("{}/a2a/sse/{}", base, params.task_id);
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution
    // (resolved from the local parent-task row, if this id is a local task).
    let pid: Option<i32> = match uuid::Uuid::parse_str(&params.task_id) {
        Ok(task_id) => sqlx::query_scalar::<_, i32>(
            "SELECT project_id FROM a2a_tasks WHERE id = $1 AND project_id IS NOT NULL",
        )
        .bind(task_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
        Err(_) => None,
    };
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "target_agent": params.target_agent,
        "task_id": params.task_id,
        "sse_url": sse_url,
        "hint": "Consumers should open an HTTP GET to sse_url with Accept: text/event-stream to receive incremental events.",
    }))
}
