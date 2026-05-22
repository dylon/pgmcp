//! `tool_panic_paths` — Per-function reachable-panic count (SOTA Phase 5.3, Cui ATC 2018).
//!
//! Reads `function_metrics.panic_paths` produced by the function-metrics cron
//! (which already counts `panic!`/`unwrap`/`expect`/`assert!` etc. per
//! function). Joins with `file_symbols` to surface paths.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::PanicPathsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_panic_paths(
    ctx: &SystemContext,
    params: PanicPathsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "panic_paths", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let entry_filter = params.entry_filter.as_deref().unwrap_or("any");
    let limit = params.limit.unwrap_or(50);

    let vis_clause = match entry_filter {
        "pub" => "AND COALESCE(fs.visibility, 'private') = 'public'",
        "module" => "AND COALESCE(fs.visibility, 'private') = 'module'",
        "private" => "AND COALESCE(fs.visibility, 'private') = 'private'",
        _ => "",
    };
    let sql = format!(
        "SELECT f.relative_path, fs.name, fs.start_line, fs.end_line, fm.panic_paths, fm.cyclomatic
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fm.project_id = $1 AND fm.panic_paths > 0 {vis_clause}
         ORDER BY fm.panic_paths DESC, fm.cyclomatic DESC
         LIMIT $2"
    );
    let rows: Vec<(String, String, i32, i32, i32, i32)> = sqlx::query_as::<
        _,
        (String, String, i32, i32, i32, i32),
    >(&sql)
    .bind(project_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Panic-paths query failed: {}", e), None))?;
    let funcs: Vec<_> = rows
        .into_iter()
        .map(|(f, n, s, e, p, c)| {
            json!({
                "file": f,
                "function": n,
                "start_line": s,
                "end_line": e,
                "panic_paths": p,
                "cyclomatic": c,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "entry_filter": entry_filter,
        "functions": funcs,
        "guidance": "Functions with high panic-leaf counts crash on unexpected input. Public functions are worst because they have no caller-controllable input validation upstream."
    }))
}
