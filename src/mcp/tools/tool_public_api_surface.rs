//! `tool_public_api_surface` — Enumerate public symbols (SOTA Phase 7.1).
//!
//! Uses `file_symbols.visibility = 'public'` from the symbol-extraction cron.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::PublicApiSurfaceParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_public_api_surface(
    ctx: &SystemContext,
    params: PublicApiSurfaceParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "public_api_surface", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let format = params.format.as_deref().unwrap_or("summary");
    let limit = params.limit.unwrap_or(500);

    type ApiRow = (String, String, String, i32, Option<String>, Option<String>);
    let rows: Vec<ApiRow> = sqlx::query_as::<_, ApiRow>(
        "SELECT f.relative_path, fs.name, fs.kind, fs.start_line, fs.signature, f.language
             FROM file_symbols fs
             JOIN indexed_files f ON fs.file_id = f.id
             WHERE f.project_id = $1
               AND fs.visibility = 'public'
               AND ($2::text IS NULL OR f.language = $2)
             ORDER BY f.relative_path, fs.start_line
             LIMIT $3",
    )
    .bind(project_id)
    .bind(params.language.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("API surface query failed: {}", e), None))?;

    let by_kind: std::collections::HashMap<String, i32> = {
        let mut m = std::collections::HashMap::new();
        for (_, _, kind, _, _, _) in &rows {
            *m.entry(kind.clone()).or_insert(0) += 1;
        }
        m
    };
    if format == "summary" {
        return json_result(&json!({
            "project": params.project,
            "total_public": rows.len(),
            "by_kind": by_kind,
            "guidance": "Aggregate counts of public symbols by kind. Use format=\"full\" for the per-symbol list."
        }));
    }
    let symbols: Vec<_> = rows
        .into_iter()
        .map(|(path, name, kind, line, sig, lang)| {
            json!({
                "file": path,
                "name": name,
                "kind": kind,
                "start_line": line,
                "signature": sig,
                "language": lang,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "total_public": symbols.len(),
        "by_kind": by_kind,
        "symbols": symbols,
    }))
}
