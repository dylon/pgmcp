//! A2A visibility tools: presence heartbeat, who-owns, per-agent activity, and
//! the workspace/plan activity feed — "who is working on what".

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    AgentActivityParams, AgentHeartbeatParams, WorkItemActivityParams, WorkItemWhoOwnsParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::collab::agent_of;
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};

/// `agent_heartbeat` — mark the agent active and renew the leases on all items
/// it currently holds (one round-trip: liveness + lease renewal).
pub async fn tool_agent_heartbeat(
    ctx: &SystemContext,
    params: AgentHeartbeatParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().agent_heartbeats.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let agent = agent_of(&params.agent_id);
    let current = match params.current_work_item_public_id.as_deref() {
        Some(p) => Some(id_of_public(pool, p).await?),
        None => None,
    };
    queries::touch_presence(pool, agent, current)
        .await
        .map_err(map_db_err)?;
    let renewed = queries::renew_agent_leases(pool, agent, params.lease_secs.unwrap_or(300))
        .await
        .map_err(map_db_err)?;
    json_result(&json!({ "agent_id": agent, "leases_renewed": renewed }))
}

/// `work_item_who_owns` — who holds an item now + the claim/handoff history.
pub async fn tool_work_item_who_owns(
    ctx: &SystemContext,
    params: WorkItemWhoOwnsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let row = queries::get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;
    let history = queries::work_item_claim_history(pool, row.id, params.limit.unwrap_or(20))
        .await
        .map_err(map_db_err)?;
    json_result(&json!({
        "public_id": row.public_id,
        "owner": row.claimed_by,
        "lease_expires_at": row.lease_expires_at,
        "claim_count": row.claim_count,
        "status": row.status,
        "history": history,
    }))
}

/// `agent_activity` — "what is agent X doing" (its presence + current items), or
/// with no `agent_id`, the active-agent roster ("who is working").
pub async fn tool_agent_activity(
    ctx: &SystemContext,
    params: AgentActivityParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let within = params.active_within_secs.unwrap_or(600);
    match params.agent_id.as_deref().filter(|s| !s.is_empty()) {
        Some(agent) => {
            let presence = queries::get_agent_presence(pool, agent)
                .await
                .map_err(map_db_err)?;
            let items = queries::agent_current_items(pool, agent)
                .await
                .map_err(map_db_err)?;
            json_result(&json!({
                "agent_id": agent,
                "presence": presence,
                "workload": items.len(),
                "current_items": items,
            }))
        }
        None => {
            let roster = queries::agent_presence_roster(pool, within)
                .await
                .map_err(map_db_err)?;
            json_result(&json!({ "active_agents": roster.len(), "roster": roster }))
        }
    }
}

/// `work_item_activity` — the workspace (or plan-scoped) activity feed: recent
/// progress + claim events, newest first, agent-attributed.
pub async fn tool_work_item_activity(
    ctx: &SystemContext,
    params: WorkItemActivityParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let root_id = match params.plan_public_id.as_deref() {
        Some(p) => Some(id_of_public(pool, p).await?),
        None => None,
    };
    let since = params
        .since
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let feed = queries::activity_feed(pool, root_id, since, params.limit.unwrap_or(50))
        .await
        .map_err(map_db_err)?;
    json_result(&json!({ "events": feed.len(), "feed": feed }))
}
