//! `topic_trends` — emerging/declining themes + quality trajectory. Thin
//! handler over [`crate::topic_analysis::trends`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::topic_analysis::render::{parse_format, render};
use crate::topic_analysis::trends::collect_topic_trends;

pub async fn tool_topic_trends(
    ctx: &SystemContext,
    params: TopicTrendsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("topic_trends requires a real PgPool", None))?;
    let scope = params
        .scope
        .as_deref()
        .unwrap_or("global")
        .trim()
        .to_string();
    let mode = params.mode.as_deref().unwrap_or("longitudinal");
    if !matches!(mode, "longitudinal" | "quality" | "chunk_age") {
        return Err(McpError::invalid_params(
            "mode must be 'longitudinal', 'quality', or 'chunk_age'",
            None,
        ));
    }
    let recent_days = params.recent_days.unwrap_or(90);

    let report = collect_topic_trends(pool, &scope, mode, recent_days)
        .await
        .map_err(|e| McpError::internal_error(format!("topic_trends: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
