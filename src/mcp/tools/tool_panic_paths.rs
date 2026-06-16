//! `tool_panic_paths` — Per-function reachable-panic count (SOTA Phase 5.3, Cui ATC 2018).
//!
//! Reads `function_metrics.panic_paths` produced by the function-metrics cron
//! (which already counts `panic!`/`unwrap`/`expect`/`assert!` etc. per
//! function). Joins with `file_symbols` to surface paths. Phase D2b adds a
//! `effect_marked` channel listing symbols whose extractor flagged them
//! with the `may_panic` effect — a precise complement to the metric-based
//! count.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::PanicPathsParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::EFFECT_MAY_PANIC;

const DEFAULT_PANIC_PATHS_LIMIT: i32 = 50;
const MAX_PANIC_PATHS_LIMIT: i32 = 1000;

pub async fn tool_panic_paths(
    ctx: &SystemContext,
    params: PanicPathsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "panic_paths", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;

    let entry_filter = params
        .entry_filter
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("any");
    if !matches!(entry_filter, "any" | "pub" | "module" | "private") {
        return Err(McpError::invalid_params(
            "entry_filter must be one of: any, pub, module, private",
            None,
        ));
    }
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PANIC_PATHS_LIMIT)
        .clamp(1, MAX_PANIC_PATHS_LIMIT);

    let vis_clause = match entry_filter {
        "pub" => "AND COALESCE(fs.visibility, 'private') = 'public'",
        "module" => "AND COALESCE(fs.visibility, 'private') = 'module'",
        "private" => "AND COALESCE(fs.visibility, 'private') = 'private'",
        _ => "",
    };
    let sql = format!(
        "SELECT f.relative_path, fs.name, fs.start_line, fs.end_line, fm.panic_paths, fm.cyclomatic
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id AND fs.file_id = fm.file_id
         JOIN indexed_files f ON f.id = fm.file_id AND f.project_id = fm.project_id
         WHERE fm.project_id = $1 AND f.project_id = $1 AND fm.panic_paths > 0 {vis_clause}
         ORDER BY fm.panic_paths DESC, fm.cyclomatic DESC
         LIMIT $2"
    );
    let rows: Vec<(String, String, i32, i32, i32, i32)> = sqlx::query_as::<
        _,
        (String, String, i32, i32, i32, i32),
    >(sqlx::AssertSqlSafe(sql.as_str()))
    .bind(project_id)
    .bind(i64::from(limit))
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
    // Shadow-ASR channel: symbols flagged with `may_panic` effect by
    // the per-language extractor. Complements `function_metrics.panic_paths`
    // (which is a count) with a yes/no boolean per symbol.
    let effect_marked = symbols_with_effect(pool, project_id, EFFECT_MAY_PANIC)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            json!({
                "symbol_id": symbol_id,
                "file_id": file_id,
                "name": name,
                "scope_path": scope_path,
            })
        })
        .collect::<Vec<_>>();

    json_result(&json!({
        "project": project,
        "entry_filter": entry_filter,
        "limit": limit,
        "functions": funcs,
        "effect_marked": effect_marked,
        "guidance": "Functions with high panic-leaf counts crash on unexpected input. Public functions are worst because they have no caller-controllable input validation upstream. The `effect_marked` channel surfaces every symbol the extractor tagged with `may_panic` — useful when you want a binary `panics?` answer rather than the count-based metric."
    }))
}
