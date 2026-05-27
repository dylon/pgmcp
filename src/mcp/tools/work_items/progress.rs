//! Progress tool bodies for the work-item tracker: append a progress note and
//! read an item's progress log (`work_item_progress`, see
//! `crate::db::migrations::v4_work_items`).
//!
//! **Trust rule:** a note authored through MCP is ALWAYS `provenance =
//! 'agent_write'` — the `user_explicit` provenance is reserved for the later
//! REST/CLI surface and is never accepted from tool params. A self-reported
//! `percent` updates the item's `claimed_percent` (shown to the user as the
//! agent's claim) but is NOT trusted for the verified roll-up.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::context::SystemContext;
use crate::db::queries::{self, insert_progress, list_progress};
use crate::mcp::server::{WorkItemProgressLogParams, WorkItemRecordProgressParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::collab::agent_of;
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};

// ============================================================================
// work_item_record_progress
// ============================================================================

pub async fn tool_work_item_record_progress(
    ctx: &SystemContext,
    params: WorkItemRecordProgressParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;

    let note = params.note.trim();
    if note.is_empty() {
        return Err(McpError::invalid_params("note must be non-empty", None));
    }

    // Clamp the self-reported percent into [0, 100] and narrow to i16 (the
    // column type); the CHECK on the table would otherwise reject out-of-range.
    let percent: Option<i16> = params.percent.map(|p| p.clamp(0, 100) as i16);

    // HARD TRUST RULE: an MCP caller is an agent, so provenance is always
    // 'agent_write' (the agent's claim, NOT trusted for the verified roll-up).
    // The agent's free-text identity IS attributed via actor_id so the activity
    // feed can show "who did what" — attribution and trust are orthogonal (the
    // trust gate is provenance + the evidence path, not the actor label).
    let agent = agent_of(&params.agent_id);
    let new_id = insert_progress(pool, item_id, note, percent, "agent_write", Some(agent))
        .await
        .map_err(map_db_err)?;

    // Progress is a strong liveness signal — keep agent_presence fresh so the
    // visibility tools never show a working agent as idle/offline between
    // heartbeats (the design's "activity-driven, never stale" presence rule).
    let _ = queries::touch_presence(pool, agent, Some(item_id)).await;

    // Return the freshly inserted row (newest first, take the top one).
    let row = list_progress(pool, item_id, 1)
        .await
        .map_err(map_db_err)?
        .into_iter()
        .find(|r| r.id == new_id)
        .ok_or_else(|| McpError::internal_error("recorded progress row vanished", None))?;

    ctx.stats()
        .work_item_progress_logged
        .fetch_add(1, Ordering::Relaxed);
    json_result(&row)
}

// ============================================================================
// work_item_progress_log
// ============================================================================

pub async fn tool_work_item_progress_log(
    ctx: &SystemContext,
    params: WorkItemProgressLogParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;
    let rows = list_progress(pool, item_id, params.limit.unwrap_or(50))
        .await
        .map_err(map_db_err)?;
    json_result(&rows)
}
