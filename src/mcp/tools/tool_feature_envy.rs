//! `tool_feature_envy` — ATFD-style feature-envy detection (SOTA Phase 10.2,
//! Lanza-Marinescu 2006).
//!
//! Per function: count distinct *external* files referenced via call edges
//! vs its own file. ATFD = external_refs / total_refs. High = envy.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::FeatureEnvyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_feature_envy(
    ctx: &SystemContext,
    params: FeatureEnvyParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "feature_envy", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let threshold = params.threshold.unwrap_or(0.6);
    let limit = params.limit.unwrap_or(30);

    let rows: Vec<(i64, String, String, i32, i32)> =
        sqlx::query_as::<_, (i64, String, String, i32, i32)>(
            "WITH src AS (
                SELECT sr.source_symbol_id AS sid,
                       sr.source_file_id   AS sfid,
                       sr.target_file_id   AS tfid
                FROM symbol_references sr
                JOIN indexed_files f ON sr.source_file_id = f.id
                WHERE f.project_id = $1 AND sr.ref_kind = 'call' AND sr.source_symbol_id IS NOT NULL
            ),
            agg AS (
                SELECT sid, sfid,
                       COUNT(*)::int AS total,
                       SUM(CASE WHEN tfid IS NOT NULL AND tfid <> sfid THEN 1 ELSE 0 END)::int AS external
                FROM src
                GROUP BY sid, sfid
            )
            SELECT a.sid, fs.name, f.relative_path, a.total, a.external
            FROM agg a
            JOIN file_symbols fs ON fs.id = a.sid
            JOIN indexed_files f ON f.id = a.sfid
            WHERE a.total >= 3",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Envy query failed: {}", e), None))?;

    let mut envies: Vec<(String, String, i32, i32, f64)> = rows
        .into_iter()
        .filter_map(|(_sid, name, path, total, ext)| {
            let atfd = ext as f64 / total.max(1) as f64;
            if atfd >= threshold {
                Some((path, name, total, ext, atfd))
            } else {
                None
            }
        })
        .collect();
    envies.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    envies.truncate(limit.max(0) as usize);
    let rows_json: Vec<_> = envies
        .iter()
        .map(|(p, n, t, e, a)| {
            json!({
                "file": p,
                "function": n,
                "total_refs": t,
                "external_refs": e,
                "atfd": a,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "threshold": threshold,
        "envies": rows_json,
        "guidance": "Functions with high ATFD reference more foreign data than their own file's. Consider moving them to the file they most envy."
    }))
}
