//! `tool_missing_preallocation` — Vec::new()/HashMap::new() followed by a
//! known-bound loop (SOTA Phase 5.7).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::MissingPreallocationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::{TAG_CONTAINER, TAG_DYNAMIC};

pub async fn tool_missing_preallocation(
    ctx: &SystemContext,
    params: MissingPreallocationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "missing_preallocation", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let pat = Regex::new(
        r"(?m)\b(Vec::new\(\)|HashMap::new\(\)|HashSet::new\(\)|BTreeMap::new\(\)|VecDeque::new\(\)|new\s+ArrayList<|new\s+HashMap<|\[\]|\{\}|list\(\)|dict\(\)|set\(\))"
    ).expect("prealloc regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit.max(0) as usize)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();
    // Shadow-ASR channel: symbols whose parameters are dynamic containers.
    let dynamic_container_symbols: Vec<serde_json::Value> =
        sqlx::query_as::<_, (i64, i64, String, Option<String>)>(
            "SELECT DISTINCT fs.id, fs.file_id, fs.name, fs.scope_path
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         JOIN symbol_parameters p ON p.symbol_id = fs.id
         WHERE f.project_id = $1
           AND p.type_tags @> ARRAY[$2, $3]::text[]
         ORDER BY fs.file_id",
        )
        .bind(project_id)
        .bind(TAG_CONTAINER)
        .bind(TAG_DYNAMIC)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            serde_json::json!({
                "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
            })
        })
        .collect();
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            let pid = crate::mcp::tools::sema_helpers::effects::project_id_opt(
                pool,
                Some(params.project.as_str()),
            )
            .await;
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
        }
        None => serde_json::json!({}),
    };

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "matches": rows,
        "dynamic_container_symbols": dynamic_container_symbols,
        "guidance": "Default empty constructors followed by loops can be preallocated when the bound is known (Vec::with_capacity, HashMap::with_capacity). Inspect surrounding code for a known size hint."
    }))
}
