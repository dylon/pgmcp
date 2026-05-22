//! Shared helpers for SOTA Phase 2-11 MCP tool bodies.
//!
//! Most tools follow the same shape: look up project_id, load the graph (or
//! query a derived metric), run the algorithm, return JSON. This module
//! exposes the common scaffolding so each tool file stays small.

#![allow(dead_code)]

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use sqlx::PgPool;

use crate::context::SystemContext;

/// Look up `projects.id` by name; returns a McpError if not found.
pub async fn project_id_or_err(ctx: &SystemContext, project: &str) -> Result<i32, McpError> {
    let pool = pool_or_err(ctx)?;
    let id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
        .bind(project)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;
    id.ok_or_else(|| McpError::internal_error(format!("Project not found: {}", project), None))
}

/// Get the pool from the DbClient or error.
pub fn pool_or_err(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error(
            "Inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            None,
        )
    })
}

/// Wrap a serializable result as a CallToolResult text content.
pub fn json_result<T: serde::Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(v)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Convenience text result.
pub fn text_result(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}
