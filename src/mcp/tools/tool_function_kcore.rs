//! `tool_function_kcore` — k-core decomposition over the call graph
//! (graph-roadmap Phase 1.1).
//!
//! Reads `function_metrics.coreness`, assigned by running the genericized
//! Batagelj-Zaversnik k-core decomposition on the symbol-resolved call graph
//! in the call-graph cron. Answers "which functions sit in the densely
//! interconnected execution core?" — the k-core with the highest k is the
//! tangled heart of the system, hardest to extract or refactor in isolation.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::FunctionKcoreParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_function_kcore(
    ctx: &SystemContext,
    params: FunctionKcoreParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "function_kcore", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let min_coreness = params.min_coreness.unwrap_or(2).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 1000);

    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, i32, i32, i32, f64, i32, i32)> = sqlx::query_as(
        "SELECT f.relative_path, fs.name, fs.start_line, fs.end_line,
                fm.coreness, fm.pagerank, fm.fan_in, fm.fan_out
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fm.project_id = $1 AND fm.coreness >= $2
         ORDER BY fm.coreness DESC, fm.pagerank DESC
         LIMIT $3",
    )
    .bind(project_id)
    .bind(min_coreness)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("function_kcore query failed: {}", e), None))?;

    let max_core = rows.iter().map(|r| r.4).max().unwrap_or(0);
    let functions: Vec<_> = rows
        .into_iter()
        .map(|(f, n, s, e, core, pr, fi, fo)| {
            json!({
                "file": f, "function": n, "start_line": s, "end_line": e,
                "coreness": core, "pagerank": pr, "fan_in": fi, "fan_out": fo,
            })
        })
        .collect();

    json_result(&json!({
        "project": params.project,
        "min_coreness": min_coreness,
        "max_coreness": max_core,
        "functions": functions,
        "guidance": if functions.is_empty() {
            "No functions at or above the requested coreness — either the `call-graph` cron has not \
             run for this project (coreness defaults to 0), or the call graph is shallow. Ensure \
             `symbol-extraction`/`function-metrics`/`call-graph` ran, or lower `min_coreness`."
        } else {
            "Coreness k means a function belongs to a maximal subgraph where every function has at \
             least k call-neighbours within it. The highest-k core is the system's tightly-woven \
             execution center: changes there ripple widely and it resists being split out. Use it to \
             locate the architectural nucleus and to prioritize the most consequential refactors."
        }
    }))
}
