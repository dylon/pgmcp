//! `tool_cochange_mutual_information` — MI over git co-change (SOTA Phase 3.2).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::info_theory::cochange_mutual_information;
use crate::mcp::server::CochangeMutualInformationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_cochange_mutual_information(
    ctx: &SystemContext,
    params: CochangeMutualInformationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "cochange_mutual_information", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let min_support = params.min_support.unwrap_or(3);
    let limit = params.limit.unwrap_or(50);

    let pairs = cochange_mutual_information(pool, project_id, min_support, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("MI query failed: {}", e), None))?;

    // Resolve file_ids to paths.
    let ids: Vec<i64> = {
        let mut set = std::collections::HashSet::new();
        for p in &pairs {
            set.insert(p.file_a);
            set.insert(p.file_b);
        }
        set.into_iter().collect()
    };
    let paths: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, relative_path FROM indexed_files WHERE id = ANY($1::bigint[])",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Path lookup failed: {}", e), None))?;
    let by_id: std::collections::HashMap<i64, String> = paths.into_iter().collect();

    let rows: Vec<_> = pairs
        .iter()
        .map(|p| {
            json!({
                "file_a": by_id.get(&p.file_a).cloned().unwrap_or_default(),
                "file_b": by_id.get(&p.file_b).cloned().unwrap_or_default(),
                "mi": p.mi,
                "support": p.support,
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
        "min_support": min_support,
        "pairs": rows,
        "guidance": "Mutual information sharpens Jaccard co-change by penalizing coincidental overlap with high-frequency files. Top pairs are causally-coupled refactor candidates."
    }))
}
