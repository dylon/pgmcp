//! `tool_centrality_analysis` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_centrality_analysis(
    ctx: &SystemContext,
    params: CentralityAnalysisParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().centrality_scans.fetch_add(1, Ordering::Relaxed);

    let metric = params.metric.as_deref().unwrap_or("all");
    let limit = params.limit.unwrap_or(20);

    debug!(
        tool = "centrality_analysis",
        project = %params.project,
        metric,
        limit,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    struct MetricRow {
        relative_path: String,
        language: String,
        pagerank: Option<f64>,
        betweenness: Option<f64>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }

    let order_clause = match metric {
        "pagerank" => "fm.pagerank DESC NULLS LAST",
        "betweenness" => "fm.betweenness DESC NULLS LAST",
        "degree" => "(COALESCE(fm.in_degree,0) + COALESCE(fm.out_degree,0)) DESC",
        _ => "fm.pagerank DESC NULLS LAST",
    };

    let query = format!(
        "SELECT f.relative_path, f.language,
                fm.pagerank, fm.betweenness, fm.in_degree, fm.out_degree
         FROM file_metrics fm
         JOIN indexed_files f ON fm.file_id = f.id
         JOIN projects p ON fm.project_id = p.id
         WHERE p.name = $1
         ORDER BY {}
         LIMIT $2",
        order_clause
    );

    let rows: Vec<MetricRow> =
        sqlx::query_as::<_, MetricRow>(&query)
            .bind(&params.project)
            .bind(limit as i64)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Metric query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No file metrics found. The graph-analysis cron job may not have run yet for this project.",
        )]));
    }

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

    let result = serde_json::json!({
        "project": params.project,
        "metric": metric,
        "file_count": files.len(),
        "files": files,
        "guidance": "High PageRank files are depended upon by many others (critical paths). \
                     High betweenness files sit on many shortest paths (bottlenecks). \
                     High degree files have many direct dependencies.",
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
