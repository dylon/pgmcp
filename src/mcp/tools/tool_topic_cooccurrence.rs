//! `topic_cooccurrence` — topic-topic coupling graph + bridge concerns. Thin
//! handler over [`crate::topic_analysis::cooccurrence`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;
use crate::topic_analysis::cooccurrence::collect_cooccurrence;
use crate::topic_analysis::render::{parse_format, render};

pub async fn tool_topic_cooccurrence(
    ctx: &SystemContext,
    params: TopicCooccurrenceParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("topic_cooccurrence requires a real PgPool", None)
    })?;
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let min_weight = params.min_weight.unwrap_or(2).max(1);

    let report = collect_cooccurrence(pool, project_id, project, min_weight)
        .await
        .map_err(|e| McpError::internal_error(format!("topic_cooccurrence: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
