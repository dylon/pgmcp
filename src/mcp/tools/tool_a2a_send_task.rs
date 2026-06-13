//! `a2a_send_task` — dispatch a Task to a registered A2A peer.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::context::SystemContext;
use crate::mcp::server::A2aSendTaskParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_send_task(
    ctx: &SystemContext,
    params: A2aSendTaskParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_send_task", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Resolve peer URL.
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

    let client = A2aClient::new(url);
    let opts = SendOptions {
        recursion_rounds: params.recursion_rounds,
        parent_task_id: None,
    };
    let task = client
        .send_task_with(&params.message, params.skill_id.as_deref(), opts)
        .await
        .map_err(|e| McpError::internal_error(format!("A2A send failed: {}", e), None))?;
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let pid =
        crate::mcp::tools::sema_helpers::effects::project_id_opt(pool, params.project.as_deref())
            .await;
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await;

    let next_hint = format!(
        "Track this task: a2a_subscribe_task(task_id='{}') for streaming updates, or \
         a2a_get_task(task_id='{}') to poll.",
        task.id, task.id
    );
    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "target_agent": params.target_agent,
        "task_id": task.id,
        "next": next_hint,
        "status": serde_json::to_value(&task.status).unwrap_or(json!({})),
        "artifacts": task.artifacts,
        "recursion_rounds": task.recursion_rounds,
        "current_round": task.current_round,
    }))
}
