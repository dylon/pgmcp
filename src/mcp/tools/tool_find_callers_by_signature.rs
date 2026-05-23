//! `tool_find_callers_by_signature` — find call sites where a target
//! function is called, filtered by the caller's signature shape.
//!
//! "All call sites where parameter N has type-tag `Mutex`" — currently
//! impossible without shadow-ASR. Uses `symbol_references` (resolved
//! edges) JOINed against `symbol_parameters.type_tags`.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::FindCallersBySignatureParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_find_callers_by_signature(
    ctx: &SystemContext,
    params: FindCallersBySignatureParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "find_callers_by_signature", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).max(1) as i64;

    let target_path = &params.target_path;
    let parameter_type_tags = params.parameter_type_tags.unwrap_or_default();
    let parameter_position = params.parameter_position;
    let caller_effects = params.caller_effects.unwrap_or_default();

    // Match callers by their (source_symbol_id)'s parameter shape.
    let sql = "
        SELECT DISTINCT sr.source_symbol_id, fs.name, fs.scope_path, f.relative_path, f.language,
                        sr.source_line, sr.target_path
        FROM symbol_references sr
        JOIN file_symbols fs ON fs.id = sr.source_symbol_id
        JOIN indexed_files f ON f.id = sr.source_file_id
        WHERE f.project_id = $1
          AND sr.target_path = $2
          AND sr.source_symbol_id IS NOT NULL
          AND ($3::text[] IS NULL OR EXISTS (
              SELECT 1 FROM symbol_parameters p
              WHERE p.symbol_id = fs.id
                AND p.type_tags @> $3::text[]
                AND ($4::int4 IS NULL OR p.position = $4::int4)
          ))
          AND ($5::text[] IS NULL OR EXISTS (
              SELECT 1 FROM symbol_effects se
              WHERE se.symbol_id = fs.id AND se.effect = ANY($5::text[])
          ))
        ORDER BY sr.source_line
        LIMIT $6::int8
    ";
    let param_arg = if parameter_type_tags.is_empty() {
        None
    } else {
        Some(parameter_type_tags.clone())
    };
    let effects_arg = if caller_effects.is_empty() {
        None
    } else {
        Some(caller_effects.clone())
    };

    type Row = (
        Option<i64>,
        String,
        Option<String>,
        String,
        String,
        i32,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(sql)
        .bind(project_id)
        .bind(target_path)
        .bind(param_arg)
        .bind(parameter_position)
        .bind(effects_arg)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    let callers: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(sid, name, scope, path, lang, line, tpath)| {
            json!({
                "symbol_id": sid,
                "name": name,
                "scope_path": scope,
                "file": path,
                "language": lang,
                "source_line": line,
                "target_path": tpath,
            })
        })
        .collect();

    json_result(&json!({
        "target_path": target_path,
        "callers": callers,
        "filters": {
            "parameter_type_tags": parameter_type_tags,
            "parameter_position": parameter_position,
            "caller_effects": caller_effects,
        },
        "guidance": "Returns resolved callers of the target path whose signature shape \
                     matches the filter. Backed by `symbol_references.target_path` + \
                     `symbol_parameters.type_tags` + `symbol_effects`."
    }))
}
