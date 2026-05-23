//! `tool_type_shape_search` — search for functions by structural type shape.
//!
//! Queries: "functions returning Result<T,_>", "handlers taking a single
//! Request<_> parameter", "async functions touching effect database".
//! Uses the GIN-indexed `return_type_tags` + `symbol_parameters.type_tags`
//! + `symbol_effects` columns. Pattern D (type-tag-aware search).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::TypeShapeSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_type_shape_search(
    ctx: &SystemContext,
    params: TypeShapeSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "type_shape_search", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).max(1) as i64;

    let return_tags = params.return_type_tags.unwrap_or_default();
    let param_tags = params.parameter_type_tags.unwrap_or_default();
    let effects = params.effects.unwrap_or_default();

    if return_tags.is_empty() && param_tags.is_empty() && effects.is_empty() {
        return json_result(&json!({
            "matches": [],
            "guidance": "Provide at least one of: return_type_tags, parameter_type_tags, effects."
        }));
    }

    // Compose a single query using array overlap operators. NULLs in
    // return_type_tags are coalesced to empty arrays so the `@>` check
    // is well-defined.
    let sql = "
        SELECT DISTINCT fs.id, fs.file_id, fs.name, fs.kind, fs.scope_path, f.relative_path, f.language,
                        COALESCE(fs.return_type_tags, '{}'::text[]) AS rtt
        FROM file_symbols fs
        JOIN indexed_files f ON f.id = fs.file_id
        WHERE f.project_id = $1
          AND ($2::text[] IS NULL OR COALESCE(fs.return_type_tags, '{}'::text[]) @> $2::text[])
          AND ($3::text[] IS NULL OR EXISTS (
              SELECT 1 FROM symbol_parameters p
              WHERE p.symbol_id = fs.id AND p.type_tags @> $3::text[]
          ))
          AND ($4::text[] IS NULL OR EXISTS (
              SELECT 1 FROM symbol_effects se
              WHERE se.symbol_id = fs.id AND se.effect = ANY($4::text[])
          ))
        ORDER BY fs.file_id, fs.start_line
        LIMIT $5::int8
    ";
    let return_arg = if return_tags.is_empty() {
        None
    } else {
        Some(return_tags.clone())
    };
    let param_arg = if param_tags.is_empty() {
        None
    } else {
        Some(param_tags.clone())
    };
    let effects_arg = if effects.is_empty() {
        None
    } else {
        Some(effects.clone())
    };

    type Row = (
        i64,
        i64,
        String,
        String,
        Option<String>,
        String,
        String,
        Vec<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(sql)
        .bind(project_id)
        .bind(return_arg)
        .bind(param_arg)
        .bind(effects_arg)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    let matches: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(sid, fid, name, kind, scope, path, lang, rtt)| {
            json!({
                "symbol_id": sid,
                "file_id": fid,
                "name": name,
                "kind": kind,
                "scope_path": scope,
                "file": path,
                "language": lang,
                "return_type_tags": rtt,
            })
        })
        .collect();

    json_result(&json!({
        "matches": matches,
        "filters": {
            "return_type_tags": return_tags,
            "parameter_type_tags": param_tags,
            "effects": effects,
        },
        "guidance": "Returns function-shaped symbols whose return_type_tags, parameter type_tags, \
                     and effects ALL match the supplied filters. Use GIN-indexed `@>` semantics: \
                     supplied tags are a SUBSET of the symbol's tags."
    }))
}
