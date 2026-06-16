//! `topic_owners` — per-topic ownership / bus-factor from git blame. Thin
//! handler over [`crate::topic_analysis::owners`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;
use crate::topic_analysis::owners::collect_topic_owners;
use crate::topic_analysis::render::{parse_format, render};

pub async fn tool_topic_owners(
    ctx: &SystemContext,
    params: TopicOwnersParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("topic_owners requires a real PgPool", None))?;
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let top_authors = params.top_authors.unwrap_or(5).clamp(1, 50);

    let report = collect_topic_owners(pool, project_id, project, top_authors)
        .await
        .map_err(|e| McpError::internal_error(format!("topic_owners: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
