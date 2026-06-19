//! `workspace_architecture_quality` — the combined inter+intra architectural
//! picture over the hierarchical rollup (ADR-027 Stage 5): the workspace summary,
//! per-group summaries, and per-project metrics, read from `project_metrics` /
//! `hier_group_metrics` (filled by the graph-analysis rollup). With
//! `rebuild=true` it first re-aggregates the group + workspace levels from the
//! existing `project_metrics` (the per-project level requires a graph-analysis
//! run).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::WorkspaceArchitectureQualityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_workspace_architecture_quality(
    ctx: &SystemContext,
    params: WorkspaceArchitectureQualityParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    if params.rebuild.unwrap_or(false) {
        crate::hierarchy::rollup::persist_group_workspace_rollup(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("rollup: {e}"), None))?;
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    // Workspace summary row.
    let workspace = sqlx::query_as::<_, (i32, i64, f64, f64, Option<f64>)>(
        "SELECT project_count, file_count, avg_instability, avg_distance, architecture_quality_score
           FROM hier_group_metrics WHERE level = 'workspace' LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("workspace row: {e}"), None))?
    .map(|(pc, fc, ai, ad, aqs)| {
        json!({"project_count": pc, "file_count": fc, "avg_instability": ai,
               "avg_distance_from_main_sequence": ad, "architecture_quality_score": aqs})
    });

    // Per-group summaries.
    let groups =
        sqlx::query_as::<_, (Option<i64>, Option<String>, i32, i64, f64, f64, Option<f64>)>(
            "SELECT ref_id, label, project_count, file_count, avg_instability, avg_distance,
                architecture_quality_score
           FROM hier_group_metrics WHERE level = 'group'
          ORDER BY architecture_quality_score ASC NULLS LAST LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("group rows: {e}"), None))?
        .into_iter()
        .map(|(id, label, pc, fc, ai, ad, aqs)| {
            json!({"group_id": id, "label": label, "project_count": pc, "file_count": fc,
               "avg_instability": ai, "avg_distance": ad, "architecture_quality_score": aqs})
        })
        .collect::<Vec<_>>();

    // Per-project metrics (worst architecture-quality first — the actionable end).
    let projects = sqlx::query_as::<_, (String, i32, i32, f64, f64, f64, Option<f64>)>(
        "SELECT p.name, pm.file_count, pm.module_count, pm.avg_instability, pm.avg_abstractness,
                pm.avg_distance, pm.architecture_quality_score
           FROM project_metrics pm JOIN projects p ON p.id = pm.project_id
          ORDER BY pm.architecture_quality_score ASC NULLS LAST LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("project rows: {e}"), None))?
    .into_iter()
    .map(|(name, fc, mc, ai, aa, ad, aqs)| {
        json!({"project": name, "file_count": fc, "module_count": mc, "avg_instability": ai,
               "avg_abstractness": aa, "avg_distance_from_main_sequence": ad,
               "architecture_quality_score": aqs})
    })
    .collect::<Vec<_>>();

    let guidance = if workspace.is_none() && projects.is_empty() {
        Some(
            "no rollup data yet — run the graph-analysis cron (trigger_cron job=\"graph-analysis\") \
             to populate project_metrics, then call again (rebuild=true aggregates groups/workspace)",
        )
    } else {
        None
    };

    json_result(&json!({
        "workspace": workspace,
        "groups": groups,
        "projects": projects,
        "guidance": guidance,
    }))
}
