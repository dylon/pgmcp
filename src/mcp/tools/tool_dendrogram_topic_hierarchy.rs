//! `tool_dendrogram_topic_hierarchy` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::Row;

use crate::context::SystemContext;
use crate::mcp::server::DendrogramTopicHierarchyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn run(
    ctx: &SystemContext,
    params: DendrogramTopicHierarchyParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
        .bind(&params.project)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("project lookup: {e}"), None))?;
    let project_id = project_id.ok_or_else(|| {
        McpError::invalid_params(format!("project not found: {}", params.project), None)
    })?;

    let row = sqlx::query(
        "SELECT ctfidf_keywords, generated_at
         FROM topic_dendrograms WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("dendrogram lookup: {e}"), None))?;

    match row {
        Some(r) => {
            let keywords: serde_json::Value = r.try_get("ctfidf_keywords").map_err(|e| {
                McpError::internal_error(format!("decode ctfidf_keywords: {e}"), None)
            })?;
            let generated_at: chrono::DateTime<chrono::Utc> = r
                .try_get("generated_at")
                .map_err(|e| McpError::internal_error(format!("decode generated_at: {e}"), None))?;
            json_result(&json!({
                "project": params.project,
                "ctfidf_keywords": keywords,
                "generated_at": generated_at.to_rfc3339(),
            }))
        }
        None => json_result(&json!({
            "project": params.project,
            "guidance": "No dendrogram persisted yet. Wait for the topic_dendrogram \
                         cron to run (default 12 h)."
        })),
    }
}
