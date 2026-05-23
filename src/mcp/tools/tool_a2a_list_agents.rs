//! `a2a_list_agents` — list registered A2A peers.

#![allow(unused_imports)]

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::A2aListAgentsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_list_agents(
    ctx: &SystemContext,
    _params: A2aListAgentsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_list_agents", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    type AgentRow = (
        String,
        String,
        Option<String>,
        String,
        serde_json::Value,
        serde_json::Value,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    );
    let rows: Vec<AgentRow> = sqlx::query_as::<_, AgentRow>(
        "SELECT name, version, description, url, capabilities, skills, registered_at, last_seen_at
         FROM a2a_agents ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("List agents failed: {}", e), None))?;

    let agents: Vec<_> = rows
        .into_iter()
        .map(
            |(name, version, description, url, capabilities, skills, reg, seen)| {
                json!({
                    "name": name,
                    "version": version,
                    "description": description,
                    "url": url,
                    "capabilities": capabilities,
                    "skills": skills,
                    "registered_at": reg,
                    "last_seen_at": seen,
                })
            },
        )
        .collect();
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
        "effect_breakdown": effect_breakdown,"agents": agents}))
}
