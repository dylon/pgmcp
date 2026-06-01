//! `tool_sync_skeleton` — inspect one symbol's ordered synchronization ops with
//! a per-op held-set annotation (explainability / drill-down for a reported
//! cycle).

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::SyncSkeletonParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_sync_skeleton(
    ctx: &SystemContext,
    params: SyncSkeletonParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "sync_skeleton", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    // Resolve the symbol: explicit symbol_id, else (file, name) lookup.
    let symbol_id = match params.symbol_id {
        Some(id) => id,
        None => {
            let (Some(file), Some(name)) = (params.file.as_deref(), params.name.as_deref()) else {
                return Err(McpError::invalid_params(
                    "provide symbol_id, or both file and name".to_string(),
                    None,
                ));
            };
            let resolved: Option<i64> = sqlx::query_scalar(
                "SELECT fs.id FROM file_symbols fs
                 JOIN indexed_files f ON f.id = fs.file_id
                 WHERE f.project_id = $1 AND f.relative_path = $2 AND fs.name = $3
                 ORDER BY fs.start_line LIMIT 1",
            )
            .bind(project_id)
            .bind(file)
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("symbol lookup failed: {e}"), None))?;
            match resolved {
                Some(id) => id,
                None => {
                    return json_result(&json!({
                        "symbol_id": null, "ops": [],
                        "note": "no symbol matched (file, name) in this project"
                    }));
                }
            }
        }
    };

    let rows = queries::sync_ops_for_symbol(pool, symbol_id)
        .await
        .map_err(|e| McpError::internal_error(format!("sync_ops fetch failed: {e}"), None))?;

    // Annotate each op with the held-set immediately after it (acquire pushes,
    // release pops the matching key) — the same model the lock-order walk uses.
    let mut held: Vec<String> = Vec::new();
    let ops_json: Vec<_> = rows
        .iter()
        .map(|r| {
            match r.op_kind.as_str() {
                "acquire" | "acquire_read" | "acquire_write" => {
                    if let Some(k) = &r.resource_key {
                        held.push(k.clone());
                    }
                }
                "release" => {
                    if let Some(k) = &r.resource_key
                        && let Some(p) = held.iter().rposition(|h| h == k)
                    {
                        held.remove(p);
                    }
                }
                _ => {}
            }
            json!({
                "seq": r.seq,
                "op_kind": r.op_kind,
                "resource_kind": r.resource_kind,
                "resource_key": r.resource_key,
                "paradigm": r.paradigm,
                "nesting_depth": r.nesting_depth,
                "line": r.line,
                "confidence": r.resource_confidence,
                "held_after": held.clone(),
            })
        })
        .collect();

    let first = rows.first();
    json_result(&json!({
        "symbol_id": symbol_id,
        "symbol_name": first.map(|r| r.symbol_name.clone()),
        "file": first.map(|r| r.relative_path.clone()),
        "op_count": rows.len(),
        "ops": ops_json,
        "guidance": "Ordered synchronization skeleton for the symbol. `held_after` is the set of \
            locks held immediately after each op; a lock acquired while another is held is the \
            unit the lock-order graph edges on."
    }))
}
