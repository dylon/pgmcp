//! `session_checkpoint_list` — list paused/resumable Crucible orchestration
//! sessions (ADR-009 PAUSE/RESUME).
//!
//! ## Boundary
//!
//! READ-only over pgmcp's OWN `orchestration_sessions` table. Returns the
//! resumable (`paused`) sessions newest-first as JSON; pgmcp never runs a shell or
//! touches the user's files. The orchestrator picks a `session_key` and calls
//! `session_checkpoint_resume` to pick the run back up.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::session_store::list_resumable;
use crate::mcp::server::SessionCheckpointListParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_session_checkpoint_list(
    ctx: &SystemContext,
    params: SessionCheckpointListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(50);
    let rows = list_resumable(pool, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("list_resumable failed: {e}"), None))?;

    let sessions: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "session_key": r.session_key,
                "id": r.id,
                "status": r.status,
                "protocol_name": r.protocol_name,
                "orchestrator_role": r.orchestrator_role,
                "task_id": r.task_id,
                "cursor": r.cursor,
                "critic_iteration": r.critic_iteration,
                "critic_phase": r.critic_phase,
                "work_item_root": r.work_item_root,
                "experiment_ids": r.experiment_ids,
                "memory_scope": r.memory_scope,
                "parent_session_id": r.parent_session_id,
                "paused_at": r.paused_at,
                "updated_at": r.updated_at,
            })
        })
        .collect();

    json_result(&json!({
        "count": sessions.len(),
        "sessions": sessions,
        "next": "resume one with session_checkpoint_resume(session_key)",
    }))
}
