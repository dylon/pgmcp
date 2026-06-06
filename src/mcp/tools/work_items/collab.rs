//! A2A collaboration tools: atomic claim / claim-next / release / handoff over
//! the shared `work_items` tree, so multiple agents can co-execute a plan
//! without stepping on each other.
//!
//! Agent identity is the canonical free-text `agent_id` (the lowercased MCP
//! `clientInfo.name`); the `#[tool]` method fills `params.agent_id` via
//! `extract_caller` when absent, exactly like `a2a_report_outcome`. A
//! successful claim/handoff also touches `agent_presence` (activity-driven).

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    WorkItemAssignParams, WorkItemClaimNextParams, WorkItemClaimParams, WorkItemHandoffParams,
    WorkItemReleaseParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err, map_op_err};
use crate::mcp::tools::work_items::nonblank;

/// Resolve the effective agent id (non-empty, else a sentinel).
pub(crate) fn agent_of(opt: &Option<String>) -> &str {
    opt.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown-agent")
}

fn required_agent_id(opt: &Option<String>) -> Result<&str, McpError> {
    match opt.as_deref() {
        Some(raw) => {
            let agent = raw.trim();
            if agent.is_empty() {
                Err(McpError::invalid_params(
                    "agent_id must be non-empty when supplied",
                    None,
                ))
            } else {
                Ok(agent)
            }
        }
        None => Ok("unknown-agent"),
    }
}

/// `work_item_claim` — atomically claim a specific item (CAS). Reports the
/// current owner if contention/blocked/terminal lost the race.
pub async fn tool_work_item_claim(
    ctx: &SystemContext,
    params: WorkItemClaimParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let agent = required_agent_id(&params.agent_id)?;
    let id = id_of_public(pool, &params.public_id).await?;
    let lease = params.lease_secs.unwrap_or(300);
    match queries::claim_work_item(pool, id, agent, lease)
        .await
        .map_err(map_db_err)?
    {
        Some(row) => {
            ctx.stats()
                .work_item_claims_succeeded
                .fetch_add(1, Ordering::Relaxed);
            let _ = queries::touch_presence(pool, agent, Some(id)).await;
            json_result(&json!({ "claimed": true, "by": agent, "item": row }))
        }
        None => {
            ctx.stats()
                .work_item_claims_contended
                .fetch_add(1, Ordering::Relaxed);
            let owner = queries::get_work_item(pool, id)
                .await
                .ok()
                .flatten()
                .and_then(|r| r.claimed_by);
            json_result(&json!({
                "claimed": false,
                "owner": owner,
                "note": "not claimable now (owned by another agent, blocked by a dependency, or terminal)",
            }))
        }
    }
}

/// `work_item_claim_next` — claim the top unclaimed, unblocked, ready item
/// (optionally within a plan subtree). SKIP LOCKED → disjoint fan-out.
pub async fn tool_work_item_claim_next(
    ctx: &SystemContext,
    params: WorkItemClaimNextParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let agent = agent_of(&params.agent_id);
    let plan_id = match params.plan_public_id.as_deref() {
        Some(p) => Some(id_of_public(pool, p).await?),
        None => None,
    };
    let lease = params.lease_secs.unwrap_or(300);
    match queries::claim_next_work_item(pool, agent, plan_id, lease)
        .await
        .map_err(map_db_err)?
    {
        Some(row) => {
            ctx.stats()
                .work_item_claims_succeeded
                .fetch_add(1, Ordering::Relaxed);
            let iid = row.id;
            let _ = queries::touch_presence(pool, agent, Some(iid)).await;
            json_result(&json!({ "claimed": true, "by": agent, "item": row }))
        }
        None => json_result(&json!({
            "claimed": false,
            "note": "no unclaimed, unblocked, ready/pending item available in scope",
        })),
    }
}

/// `work_item_release` — release a claim (owner-gated).
pub async fn tool_work_item_release(
    ctx: &SystemContext,
    params: WorkItemReleaseParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let agent = agent_of(&params.agent_id);
    let id = id_of_public(pool, &params.public_id).await?;
    match queries::release_work_item(pool, id, agent)
        .await
        .map_err(map_db_err)?
    {
        Some(row) => {
            // The agent is still alive — it just dropped this item. Keep its
            // presence fresh (no current item) so the roster stays accurate.
            let _ = queries::touch_presence(pool, agent, None).await;
            json_result(&json!({ "released": true, "item": row }))
        }
        None => Err(McpError::invalid_params(
            "cannot release: you are not the current owner (or the item is unclaimed)",
            None,
        )),
    }
}

/// `work_item_handoff` — hand a claim to another agent (owner-gated re-key).
pub async fn tool_work_item_handoff(
    ctx: &SystemContext,
    params: WorkItemHandoffParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let agent = agent_of(&params.agent_id);
    let to = params.to_agent.trim();
    if to.is_empty() {
        return Err(McpError::invalid_params("to_agent must be non-empty", None));
    }
    let id = id_of_public(pool, &params.public_id).await?;
    let lease = params.lease_secs.unwrap_or(300);
    match queries::handoff_work_item(pool, id, agent, to, lease)
        .await
        .map_err(map_db_err)?
    {
        Some(row) => {
            ctx.stats()
                .work_item_handoffs
                .fetch_add(1, Ordering::Relaxed);
            let _ = queries::touch_presence(pool, agent, None).await;
            let _ = queries::touch_presence(pool, to, Some(id)).await;
            json_result(&json!({ "handed_off_to": to, "item": row }))
        }
        None => Err(McpError::invalid_params(
            "cannot hand off: you are not the current owner",
            None,
        )),
    }
}

/// `work_item_assign` — set (or clear) an item's DURABLE `assignee`.
///
/// `assignee` is durable ownership intent (1:1, never auto-expires, surfaced by
/// the `my-work` smart-view); it is ORTHOGONAL to `claimed_by`, the ephemeral
/// execution lease taken by `work_item_claim`. Assignment is NOT a status
/// transition — `assign_work_item` only writes the assignee columns. An empty
/// or omitted `assignee` UNASSIGNS the item.
pub async fn tool_work_item_assign(
    ctx: &SystemContext,
    params: WorkItemAssignParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let id = id_of_public(pool, &params.public_id).await?;
    // Empty/None ⇒ unassign (assign_work_item clears assigned_at when NULL).
    let assignee = nonblank(&params.assignee);
    let assigned_by = nonblank(&params.assigned_by);

    let row = queries::assign_work_item(pool, id, assignee, assigned_by)
        .await
        .map_err(map_op_err)?;
    json_result(&json!({ "assigned": assignee.is_some(), "item": row }))
}
