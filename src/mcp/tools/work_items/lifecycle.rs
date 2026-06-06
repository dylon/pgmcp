//! Status-transition tool body for the work-item tracker.
//!
//! **Hard trust rule:** the actor is ALWAYS
//! [`crate::tracker::transition::Actor::Agent`]. It is never read from params.
//! Consequently an agent requesting `verified`/`deferred`/`rejected` is refused
//! by [`crate::db::queries::set_work_item_status`]'s transition gate (mapped to
//! `invalid_params` with the explanatory message) — those transitions belong to
//! the user/gatekeeper/evidence paths, not the authoring agent.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::context::SystemContext;
use crate::db::queries::{WorkItemOpError, get_work_item_by_public_id, set_work_item_status};
use crate::mcp::server::WorkItemSetStatusParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::Actor;

// ============================================================================
// work_item_set_status
// ============================================================================

pub async fn tool_work_item_set_status(
    ctx: &SystemContext,
    params: WorkItemSetStatusParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let public_id = params.public_id.trim();
    if public_id.is_empty() {
        return Err(McpError::invalid_params(
            "public_id must be non-empty",
            None,
        ));
    }
    let status = params.status.trim();
    if status.is_empty() {
        return Err(McpError::invalid_params("status must be non-empty", None));
    }

    // Resolve the target item by public_id.
    let row = get_work_item_by_public_id(pool, public_id)
        .await
        .map_err(|e| McpError::internal_error(format!("db error: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params(format!("no work item '{public_id}'"), None))?;

    // Parse the requested status against the closed lifecycle vocabulary.
    let to = WorkItemStatus::parse(status).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "unknown status '{}'; expected one of {}",
                status,
                crate::tracker::status::sql_in_list()
            ),
            None,
        )
    })?;
    let reason = params
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());

    // HARD TRUST RULE: actor is always Agent. No evidence/negotiation is passed,
    // so a request for verified/deferred/rejected is correctly refused by the
    // transition gate (surfaced as invalid_params).
    let updated = set_work_item_status(pool, row.id, to, Actor::Agent, None, reason, None, None)
        .await
        .map_err(|e| match e {
            WorkItemOpError::Transition(_) | WorkItemOpError::NotFound => {
                McpError::invalid_params(e.to_string(), None)
            }
            WorkItemOpError::Db(_) => McpError::internal_error(e.to_string(), None),
        })?;

    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    json_result(&updated)
}
