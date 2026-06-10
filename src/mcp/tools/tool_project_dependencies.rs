//! `tool_project_dependencies` — the projects a given project depends ON (the
//! forward dependency edge). Backs the coordination scenario: "my build broke —
//! which of my dependencies might be the cause, and who is editing them?".

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_project_dependencies(
    ctx: &SystemContext,
    params: ProjectDependenciesParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let id = project_id_or_err(ctx, &params.project).await?;

    let rows = crate::deps::store::dependencies_of(pool, id)
        .await
        .map_err(|e| McpError::internal_error(format!("dependencies query failed: {e}"), None))?;
    let dependencies: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "project": r.dependency_name,
                "project_id": r.dependency_project_id,
                "dep_name": r.dep_name,
                "kind": r.kind,
                "source": r.source,
                "confidence": r.confidence,
            })
        })
        .collect();
    debug!(
        tool = "project_dependencies",
        count = dependencies.len(),
        "queried"
    );
    let body = serde_json::to_string_pretty(&json!({
        "project": params.project,
        "dependency_count": dependencies.len(),
        "dependencies": dependencies,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
