//! `tool_identifier_entropy` — Shannon entropy over identifier tokens
//! (SOTA Phase 3.4, Abebe et al. ICPC 2009).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::info_theory::identifier_entropy;
use crate::mcp::server::IdentifierEntropyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_identifier_entropy(
    ctx: &SystemContext,
    params: IdentifierEntropyParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "identifier_entropy", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let mut entries = identifier_entropy(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("Entropy query failed: {}", e), None))?;
    let limit = params.limit.unwrap_or(30);
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

    let ids: Vec<i64> = entries.iter().map(|e| e.file_id).collect();
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
                "file": by_id.get(&e.file_id).cloned().unwrap_or_default(),
                "entropy": e.entropy,
                "n_tokens": e.n_tokens,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "sort": sort,
        "files": files,
        "guidance": "Low identifier entropy = naming pollution / generated code (most names are duplicates); high = diverse domain vocabulary."
    }))
}
