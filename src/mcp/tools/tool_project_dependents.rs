//! `tool_project_dependents` — the projects that depend ON a given project (the
//! reverse dependency edge). Backs the coordination scenario: "who builds on the
//! thing I'm editing, and might break if I destabilize it?".

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_project_dependents(
    ctx: &SystemContext,
    params: ProjectDependentsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };
    let id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
        .bind(&params.project)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("project lookup: {e}"), None))?;
    let Some(id) = id else {
        return Err(McpError::invalid_params(
            format!("unknown project '{}'", params.project),
            None,
        ));
    };

    let rows = crate::deps::store::dependents_of(pool, id)
        .await
        .map_err(|e| McpError::internal_error(format!("dependents query failed: {e}"), None))?;
    let dependents: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "project": r.dependent_name,
                "project_id": r.dependent_project_id,
                "dep_name": r.dep_name,
                "kind": r.kind,
                "source": r.source,
                "confidence": r.confidence,
            })
        })
        .collect();
    debug!(
        tool = "project_dependents",
        count = dependents.len(),
        "queried"
    );
    let body = serde_json::to_string_pretty(&json!({
        "project": params.project,
        "dependent_count": dependents.len(),
        "dependents": dependents,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
