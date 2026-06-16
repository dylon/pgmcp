//! `tool_central_functions` — function-level centrality over the call graph
//! (graph-roadmap Phase 1.1).
//!
//! Reads the centrality columns materialized on `function_metrics` by the
//! call-graph cron, which now runs the genericized PageRank / Brandes
//! betweenness / harmonic-centrality / k-core algorithms over the
//! symbol-resolved *call* graph — not just the file import graph. Answers
//! "which *functions* are the load-bearing hubs of execution?", a sharper
//! question than file-level PageRank since a hub file may hold many trivial
//! functions.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CentralFunctionsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_central_functions(
    ctx: &SystemContext,
    params: CentralFunctionsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "central_functions", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(50).clamp(1, 1000);
    // Whitelist the ranking column — never interpolate user text into SQL.
    let metric = params.metric.as_deref().unwrap_or("pagerank");
    let order_col = match metric {
        "pagerank" => "pagerank",
        "betweenness" => "betweenness",
        "harmonic" => "harmonic",
        "coreness" => "coreness",
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "Unknown metric '{}'. Use one of: pagerank, betweenness, harmonic, coreness.",
                    other
                ),
                None,
            ));
        }
    };

    let sql = format!(
        "SELECT f.relative_path, fs.name, fs.start_line, fs.end_line,
                fm.pagerank, fm.betweenness, fm.harmonic, fm.coreness,
                fm.community_id, fm.fan_in, fm.fan_out, fm.cyclomatic
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fm.project_id = $1
         ORDER BY fm.{order_col} DESC NULLS LAST, fm.pagerank DESC
         LIMIT $2"
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        i32,
        i32,
        f64,
        f64,
        f64,
        i32,
        i32,
        i32,
        i32,
        i32,
    )> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(project_id)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("central_functions query failed: {}", e), None)
        })?;

    let functions: Vec<_> = rows
        .into_iter()
        .map(|(f, n, s, e, pr, bt, hm, core, comm, fi, fo, cy)| {
            json!({
                "file": f, "function": n, "start_line": s, "end_line": e,
                "pagerank": pr, "betweenness": bt, "harmonic": hm,
                "coreness": core, "community_id": comm,
                "fan_in": fi, "fan_out": fo, "cyclomatic": cy,
            })
        })
        .collect();

    let ready = functions
        .iter()
        .any(|f| f["pagerank"].as_f64().unwrap_or(0.0) > 0.0);

    json_result(&json!({
        "project": params.project,
        "metric": metric,
        "functions": functions,
        "guidance": if ready {
            "Functions ranked by call-graph centrality. PageRank = global execution importance; \
             betweenness = brokerage (control/data must flow through it); harmonic = reach; \
             coreness = embedding in the densely-interconnected execution core. High-centrality \
             functions are the riskiest to change and the best to read first when learning the code."
        } else {
            "All centralities are 0 — the `call-graph` cron has not populated function_metrics for \
             this project yet (or the call graph has no resolved edges). Ensure `symbol-extraction` \
             and `function-metrics` ran, then trigger `call-graph` and retry."
        }
    }))
}
