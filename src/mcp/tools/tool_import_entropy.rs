//! `tool_import_entropy` — H(target | source) over import edges (SOTA Phase 3.3).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::info_theory::import_conditional_entropy;
use crate::mcp::server::ImportEntropyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_import_entropy(
    ctx: &SystemContext,
    params: ImportEntropyParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "import_entropy", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let mut entries = import_conditional_entropy(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("Entropy query failed: {}", e), None))?;
    let limit = params.limit.unwrap_or(30);

    // Sort by selected order
    let sort = params.sort.as_deref().unwrap_or("entropy_desc");
    match sort {
        "entropy_asc" => entries.sort_by(|a, b| {
            a.entropy
                .partial_cmp(&b.entropy)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => entries.sort_by(|a, b| {
            b.entropy
                .partial_cmp(&a.entropy)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
    entries.truncate(limit.max(0) as usize);

    let ids: Vec<i64> = entries.iter().map(|e| e.source_file_id).collect();
    let paths: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, relative_path FROM indexed_files WHERE id = ANY($1::bigint[])",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Path lookup failed: {}", e), None))?;
    let by_id: std::collections::HashMap<i64, String> = paths.into_iter().collect();

    let files: Vec<_> = entries
        .iter()
        .map(|e| {
            json!({
                "file": by_id.get(&e.source_file_id).cloned().unwrap_or_default(),
                "entropy": e.entropy,
                "n_imports": e.n_imports,
            })
        })
        .collect();
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "sort": sort,
        "files": files,
        "guidance": "High H(target|source) = imports spread across many targets (broker / coordinator role, possible abstraction-leak); low = focused dependency."
    }))
}
