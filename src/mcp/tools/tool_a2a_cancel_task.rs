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

    // Journal the per-task cancel to the control-plane audit (ADR-020/D4): which task
    // was abandoned, on which peer. Best-effort — a journal failure must not fail the
    // cancel (which has already taken effect on the peer).
    let entry = crate::csm::trace_store::ControlInput {
        action: crate::csm::trace_store::ControlAction::Cancel,
        scope: crate::csm::trace_store::ControlScope::Task,
        session_key: None,
        task_id: Some(task_id),
        work_item_public_id: None,
        trace_id: None,
        span_id: None,
        reason: None,
        actor: Some("mcp".to_string()),
        attributes: json!({ "target_agent": params.target_agent }),
    };
    if let Err(e) = crate::csm::trace_store::record_control(pool, &entry).await {
        tracing::warn!(error = %e, "cancel control-journal append failed (cancel still applied)");
    }
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution
    // (resolved from the local parent-task row, if this id is a local task).
    let pid: Option<i32> = sqlx::query_scalar::<_, i32>(
        "SELECT project_id FROM a2a_tasks WHERE id = $1 AND project_id IS NOT NULL",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,"target_agent": params.target_agent, "task": task}))
}
