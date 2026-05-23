//! `tool_api_stability` — Per-public-symbol signature-change frequency
//! across git history (SOTA Phase 7.4, Bogart EMSE 2016).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::ApiStabilityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_api_stability(
    ctx: &SystemContext,
    params: ApiStabilityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "api_stability", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    // Count distinct (file, function_signature) changes per public symbol
    // across git_commit_chunks. The chunk_text is the commit diff hunk.
    let window = params.window_commits.unwrap_or(100) as i64;
    let rows: Vec<(String,)> = sqlx::query_as::<_, (String,)>(
        "SELECT gcc.chunk_text
         FROM git_commits gc
         JOIN git_commit_chunks gcc ON gcc.commit_id = gc.id
         WHERE gc.project_id = $1
         ORDER BY gc.committed_at DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(window)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Commit chunk query failed: {}", e), None))?;

    let sig_re = Regex::new(
        r"(?m)\+\s*pub(?:\(crate\))?\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^)]*\)|@@.*\+(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(|^\+\s*export\s+function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("
    ).expect("sig regex");

    let mut changes: HashMap<String, u32> = HashMap::new();
    for (text,) in &rows {
        for cap in sig_re.captures_iter(text) {
            for i in 1..=3 {
                if let Some(n) = cap.get(i) {
                    *changes.entry(n.as_str().to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut rows_out: Vec<(String, u32, f64)> = changes
        .into_iter()
        .map(|(name, c)| {
            let stability = 1.0 / (1.0 + c as f64 / (window.max(1) as f64));
            (name, c, stability)
        })
        .collect();
    rows_out.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let limit = params.limit.unwrap_or(50);
    rows_out.truncate(limit.max(0) as usize);
    let syms: Vec<_> = rows_out
        .iter()
        .map(|(n, c, s)| json!({"name": n, "change_count": c, "stability_score": s}))
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
        "window_commits": window,
        "symbols": syms,
        "guidance": "Bogart EMSE 2016: stability = 1 / (1 + change_count/window). Low score = signature churn — these APIs predict ecosystem breakage."
    }))
}
