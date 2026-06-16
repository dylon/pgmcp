//! `topic_coverage_gaps` — orphan / thin / low-cohesion topics, per project.
//! Thin handler over [`crate::topic_analysis::gaps`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;
use crate::topic_analysis::gaps::{CoverageGapsReport, collect_project_gaps};
use crate::topic_analysis::render::{parse_format, render};

pub async fn tool_topic_coverage_gaps(
    ctx: &SystemContext,
    params: TopicCoverageGapsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("topic_coverage_gaps requires a real PgPool", None)
    })?;
    let thin_threshold = params.thin_threshold.unwrap_or(5).max(1);
    let low_sim = params.low_sim.unwrap_or(0.2);
    let quality = crate::db::queries::get_topic_quality(pool).await;

    let targets: Vec<(i32, String)> = match params.project.as_deref() {
        Some(name) => vec![(
            project_id_or_err(ctx, name.trim()).await?,
            name.trim().to_string(),
        )],
        None => crate::db::queries::list_projects(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("list_projects: {e}"), None))?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect(),
    };

    let mut projects = Vec::with_capacity(targets.len());
    for (pid, name) in &targets {
        let g = collect_project_gaps(pool, *pid, name, thin_threshold, low_sim, &quality)
            .await
            .map_err(|e| McpError::internal_error(format!("coverage_gaps {name}: {e}"), None))?;
        projects.push(g);
    }

    let report = CoverageGapsReport { projects };
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
