//! `topic_project_map` — cross-project theme overlap from the global roll-up.
//! Thin handler over [`crate::topic_analysis::project_map`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::topic_analysis::project_map::collect_project_map;
use crate::topic_analysis::render::{parse_format, render};

pub async fn tool_topic_project_map(
    ctx: &SystemContext,
    params: TopicProjectMapParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("topic_project_map requires a real PgPool", None)
    })?;
    let min_breadth = params.min_breadth.unwrap_or(2).max(1);

    let report = collect_project_map(pool, min_breadth)
        .await
        .map_err(|e| McpError::internal_error(format!("topic_project_map: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
