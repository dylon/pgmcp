//! `project_topic_similarity` — cluster projects by topic similarity and flag
//! redundant forks/backups. Thin handler over [`crate::topic_analysis::similarity`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::topic_analysis::render::{parse_format, render};
use crate::topic_analysis::similarity::collect_similarity;

pub async fn tool_project_topic_similarity(
    ctx: &SystemContext,
    params: ProjectTopicSimilarityParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("project_topic_similarity requires a real PgPool", None)
    })?;

    let method = params.method.as_deref().unwrap_or("centroid");
    if method != "centroid" && method != "global_jsd" {
        return Err(McpError::invalid_params(
            "method must be 'centroid' or 'global_jsd'",
            None,
        ));
    }
    let threshold = params.threshold.unwrap_or(0.85);
    if !threshold.is_finite() {
        return Err(McpError::invalid_params("threshold must be finite", None));
    }

    let projects: Vec<(i32, String)> = crate::db::queries::list_projects(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("list_projects: {e}"), None))?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();

    let report = collect_similarity(pool, &projects, method, threshold.clamp(-1.0, 1.0))
        .await
        .map_err(|e| McpError::internal_error(format!("project_topic_similarity: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
