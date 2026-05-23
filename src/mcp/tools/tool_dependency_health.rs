//! `tool_dependency_health` — external/unresolved-import audit.
//!
//! Groups `code_graph_edges` rows where `target_file_id IS NULL` (external
//! crates, system libs, or Go/Java/C/C++ targets pre-Tier-0e) by `target_raw`
//! and ranks by usage centrality + staleness. Emits per-dep recommendations:
//! prune / upgrade / consolidate / keep.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_dependency_health(
    ctx: &SystemContext,
    params: DependencyHealthParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .dependency_health_scans
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(50).max(1);
    let _worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");
    let pool = pool_or_err(ctx)?;

    // Resolve project_id when provided.
    let project_id: Option<i32> = if let Some(name) = &params.project {
        sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?
    } else {
        None
    };

    debug!(
        tool = "dependency_health",
        project = params.project.as_deref().unwrap_or("*"),
        limit,
        "MCP tool invoked",
    );

    let rows = queries::find_unresolved_dependencies(pool, project_id, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("Dependency query failed: {}", e), None))?;

    let mut deps: Vec<serde_json::Value> = Vec::new();
    for r in &rows {
        let recommendation = if r.importer_count == 1
            && r.latest_change_days.unwrap_or(0.0) > 365.0
            && r.usage_centrality < 0.001
        {
            "prune"
        } else if r.importer_count >= 5 && r.latest_change_days.unwrap_or(0.0) > 365.0 {
            "upgrade"
        } else if r.importer_count >= 3 {
            "consolidate"
        } else {
            "keep"
        };
        deps.push(json!({
            "target_raw": r.target_raw,
            "importers": r.importer_count,
            "usage_centrality": format!("{:.6}", r.usage_centrality),
            "latest_importer_change_days": r.latest_change_days.map(|d| d.round() as i64),
            "recommendation": recommendation,
            "rationale": dep_rationale(recommendation, r),
            "sample_importers": r.sample_importers.clone(),
        }));
    }

    // Shadow-ASR channel: per-effect counts (project-scoped if a project
    // was given, otherwise skipped — `effect_counts` is per-project).
    let effect_breakdown = match project_id {
        Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect::<Vec<serde_json::Value>>(),
        None => Vec::new(),
    };

    let result = json!({
        "scope": if project_id.is_some() { "project" } else { "workspace" },
        "project_filter": params.project,
        "deps": deps,
        "effect_breakdown": effect_breakdown,
        "guidance": "Per-dep recommendations: prune (single importer, idle, low centrality), \
                     upgrade (high centrality + idle), consolidate (multiple importers — fuzzy \
                     consolidation is a follow-up), or keep. The `effect_breakdown` channel surfaces project-wide effect counts so reviewers can see what effects are concentrated in the dependency graph.",
        "health": json!({
            "edges_present": !rows.is_empty(),
        }),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "dependency_health",
        deps = rows.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn dep_rationale(verdict: &str, r: &queries::UnresolvedDepRow) -> String {
    match verdict {
        "prune" => format!(
            "Single importer, {:.0} days idle, low centrality — likely safe to drop.",
            r.latest_change_days.unwrap_or(0.0)
        ),
        "upgrade" => format!(
            "{} importers but stale ({:.0} days) — verify upstream is still maintained or pin a \
             specific version.",
            r.importer_count,
            r.latest_change_days.unwrap_or(0.0)
        ),
        "consolidate" => format!(
            "{} importers — consider standardizing usage patterns or extracting a wrapper.",
            r.importer_count
        ),
        _ => "Active and singular — keep as-is.".to_string(),
    }
}
