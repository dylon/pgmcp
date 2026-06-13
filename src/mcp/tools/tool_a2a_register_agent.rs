//! `a2a_register_agent` — add a peer agent to the local directory.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::A2aRegisterAgentParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_register_agent(
    ctx: &SystemContext,
    params: A2aRegisterAgentParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_register_agent", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let version = params.version.unwrap_or_else(|| "unknown".to_string());
    let capabilities = params.capabilities.unwrap_or_else(|| json!({}));
    let skills = params.skills.unwrap_or_else(|| json!([]));
    let specialty = params.specialty.unwrap_or_default();

    sqlx::query(
        "INSERT INTO a2a_agents
            (name, version, description, url, capabilities, skills,
             specialty, recommended_role)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (name) DO UPDATE SET
             version = EXCLUDED.version,
             description = EXCLUDED.description,
             url = EXCLUDED.url,
             capabilities = EXCLUDED.capabilities,
             skills = EXCLUDED.skills,
             specialty = EXCLUDED.specialty,
             recommended_role = EXCLUDED.recommended_role,
             last_seen_at = NOW()",
    )
    .bind(&params.name)
    .bind(&version)
    .bind(&params.description)
    .bind(&params.url)
    .bind(&capabilities)
    .bind(&skills)
    .bind(&specialty)
    .bind(&params.recommended_role)
    .execute(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Registration failed: {}", e), None))?;

    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let pid =
        crate::mcp::tools::sema_helpers::effects::project_id_opt(pool, params.project.as_deref())
            .await;
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "registered": params.name,
        "url": params.url,
        "specialty": specialty,
        "recommended_role": params.recommended_role,
    }))
}
