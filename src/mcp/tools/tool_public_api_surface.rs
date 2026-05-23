//! `tool_public_api_surface` — Enumerate public symbols (SOTA Phase 7.1).
//!
//! Reads `file_symbols.visibility = 'public'` from the symbol-extraction
//! cron. As of the shadow-ASR upgrade (Phase D2b), `format="full"`
//! enriches each row with the structured shadow-ASR fields
//! (`parameters`, `return_type`, `effects`, `type_tags`, `scope_path`)
//! fetched via `sema_helpers::signatures::fetch_signature_descriptor`.
//! When the helper returns `None` (legacy data from before the migration),
//! the row falls back to the raw `signature` text alone — the response
//! stays well-formed.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::PublicApiSurfaceParams;
use crate::mcp::tools::sema_helpers::signatures::fetch_signature_descriptor;
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

    type ApiRow = (
        i64,
        String,
        String,
        String,
        i32,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<ApiRow> = sqlx::query_as::<_, ApiRow>(
        "SELECT fs.id, f.relative_path, fs.name, fs.kind, fs.start_line, fs.signature, f.language
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
        for (_, _, _, kind, _, _, _) in &rows {
            *m.entry(kind.clone()).or_insert(0) += 1;
        }
        m
    };
    if format == "summary" {
        return json_result(&json!({
            "project": params.project,
            "total_public": rows.len(),
            "by_kind": by_kind,
            "guidance": "Aggregate counts of public symbols by kind. Use format=\"full\" for the per-symbol list including shadow-ASR signature descriptors when available."
        }));
    }

    // Full format: enrich each row with the shadow-ASR signature
    // descriptor. The fetch is one round-trip per symbol; for the common
    // `limit = 500` cap this stays under a second on the local DB.
    let mut symbols: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for (id, path, name, kind, line, sig, lang) in rows {
        // Skip fetching descriptors for non-function-shaped kinds since
        // their shadow-ASR fields are uniformly empty (the extraction
        // only populates parameters/return_type on Functions).
        let descriptor = if kind == "function" {
            fetch_signature_descriptor(pool, id).await.ok().flatten()
        } else {
            None
        };
        let mut row = json!({
            "file": path,
            "name": name,
            "kind": kind,
            "start_line": line,
            "signature": sig,
            "language": lang,
        });
        if let Some(d) = descriptor
            && let Some(obj) = row.as_object_mut()
        {
            obj.insert("scope_path".into(), json!(d.scope_path));
            obj.insert("scope_depth".into(), json!(d.scope_depth));
            obj.insert(
                "parameters".into(),
                serde_json::to_value(&d.parameters).unwrap_or(serde_json::Value::Array(Vec::new())),
            );
            obj.insert(
                "return_type".into(),
                json!({
                    "type_raw": d.return_type_raw,
                    "type_tags": d.return_type_tags,
                    "type_shape": d.return_type_shape,
                }),
            );
            obj.insert("effects".into(), json!(d.effects));
            obj.insert("generic_params".into(), d.generic_params);
        }
        symbols.push(row);
    }
    json_result(&json!({
        "project": params.project,
        "total_public": symbols.len(),
        "by_kind": by_kind,
        "symbols": symbols,
    }))
}
