//! `tool_centrality_analysis` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::CentralityAnalysisParams;

const MAX_CENTRALITY_RESULTS: i32 = 200;

pub async fn tool_centrality_analysis(
    ctx: &SystemContext,
    params: CentralityAnalysisParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().centrality_scans.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }

    let metric = params
        .metric
        .as_deref()
        .map(str::trim)
        .filter(|metric| !metric.is_empty())
        .unwrap_or("all");
    let order_clause = match metric {
        "all" | "pagerank" => "fm.pagerank DESC NULLS LAST",
        "betweenness" => "fm.betweenness DESC NULLS LAST",
        "degree" => "(COALESCE(fm.in_degree,0) + COALESCE(fm.out_degree,0)) DESC",
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "unknown metric '{other}'; expected one of pagerank, betweenness, degree, all"
                ),
                None,
            ));
        }
    };
    let limit = params.limit.unwrap_or(20).clamp(1, MAX_CENTRALITY_RESULTS);

    debug!(
        tool = "centrality_analysis",
        project, metric, limit, "MCP tool invoked",
    );

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))?;

    // Resolve display names fail-closed. Workspaces can contain several
    // projects with the same basename; centrality rows and enrichment must all
    // use one resolved project id, never a name join that merges them.
    let matching_project_ids: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM projects WHERE name = $1 ORDER BY id")
            .bind(project)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

    let project_id = match matching_project_ids.as_slice() {
        [] => {
            return Err(McpError::internal_error(
                format!("Project not found: {}", project),
                None,
            ));
        }
        [project_id] => *project_id,
        ids => {
            return Err(McpError::invalid_params(
                format!(
                    "ambiguous project name '{}' matched {} indexed projects; use a unique project name from list_projects",
                    project,
                    ids.len()
                ),
                None,
            ));
        }
    };

    #[derive(sqlx::FromRow)]
    struct MetricRow {
        relative_path: String,
        language: String,
        pagerank: Option<f64>,
        betweenness: Option<f64>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }

    let query = format!(
        "SELECT f.relative_path, f.language,
                fm.pagerank, fm.betweenness, fm.in_degree, fm.out_degree
         FROM file_metrics fm
         JOIN indexed_files f ON fm.file_id = f.id
         WHERE fm.project_id = $1 AND f.project_id = $1
         ORDER BY {}
         LIMIT $2",
        order_clause
    );

    let rows: Vec<MetricRow> = sqlx::query_as::<_, MetricRow>(sqlx::AssertSqlSafe(query.as_str()))
        .bind(project_id)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Metric query failed: {}", e), None))?;

    let files: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let total_degree = r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0);
            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "pagerank": r.pagerank.map(|v| format!("{:.6}", v)),
                "betweenness": r.betweenness.map(|v| format!("{:.6}", v)),
                "in_degree": r.in_degree.unwrap_or(0),
                "out_degree": r.out_degree.unwrap_or(0),
                "total_degree": total_degree,
            })
        })
        .collect();

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    // Cross-project neighborhood (ADR-009 §4.2): which projects depend on this
    // one (its changes ripple to them) and which it depends on. Centrality is
    // intra-project; this adds the project-level criticality context.
    let (cross_project_dependencies, cross_project_dependents) =
        crate::deps::store::cross_project_blocks(pool, project_id).await;

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "metric": metric,
        "limit": limit,
        "file_count": files.len(),
        "files": files,
        "cross_project_dependency_count": cross_project_dependencies.len(),
        "cross_project_dependencies": cross_project_dependencies,
        "cross_project_dependent_count": cross_project_dependents.len(),
        "cross_project_dependents": cross_project_dependents,
        "guidance": "High PageRank files are depended upon by many others (critical paths). \
                     High betweenness files sit on many shortest paths (bottlenecks). \
                     High degree files have many direct dependencies. \
                     `cross_project_dependents` are OTHER projects whose builds this one's \
                     central files can break — coordinate via a2a_active_agents.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "centrality_analysis",
        results = files.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
