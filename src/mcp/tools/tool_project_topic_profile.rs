//! `project_topic_profile` — per-project topic fingerprint (specialization
//! index, dominant topics, coherence). Thin handler over
//! [`crate::topic_analysis::profile`].

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;
use crate::topic_analysis::profile::{ProfileReport, collect_project_profile};
use crate::topic_analysis::render::{parse_format, render};

pub async fn tool_project_topic_profile(
    ctx: &SystemContext,
    params: ProjectTopicProfileParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let fmt =
        parse_format(params.format.as_deref()).map_err(|e| McpError::invalid_params(e, None))?;
    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("project_topic_profile requires a real PgPool", None)
    })?;
    let top_n = params.top_n.unwrap_or(10).clamp(1, 100);
    let quality = crate::db::queries::get_topic_quality(pool).await;

    let targets: Vec<(i32, String)> = match params.project.as_deref() {
        Some(name) => {
            let pid = project_id_or_err(ctx, name.trim()).await?;
            vec![(pid, name.trim().to_string())]
        }
        None => crate::db::queries::list_projects(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("list_projects: {e}"), None))?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect(),
    };

    let single = params.project.is_some();
    let mut profiles = Vec::with_capacity(targets.len());
    for (pid, name) in &targets {
        let p = collect_project_profile(pool, *pid, name, top_n, &quality)
            .await
            .map_err(|e| McpError::internal_error(format!("profile {name}: {e}"), None))?;
        // In the all-projects view, drop projects with no topic assignments yet.
        if !single && p.n_topics == 0 {
            continue;
        }
        profiles.push(p);
    }

    let report = ProfileReport { projects: profiles };
    Ok(CallToolResult::success(vec![Content::text(render(
        &report, fmt,
    ))]))
}
