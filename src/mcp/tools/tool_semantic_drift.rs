//! `tool_semantic_drift` — Per-symbol semantic drift across git history
//! (SOTA Phase 8.2, Hamilton et al. ACL 2016 "Diachronic Word Embeddings").
//!
//! For each public symbol, compares the embedding of its current chunk vs the
//! embedding of the oldest commit chunk that touches it. Large cosine
//! distance = silent semantic change (same name, different behaviour).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::SemanticDriftParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_semantic_drift(
    ctx: &SystemContext,
    params: SemanticDriftParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "semantic_drift", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(30);

    // Phase 5 C7: signature-aware column resolution. Both file_chunks
    // and git_commit_chunks have parallel embedding/embedding_v2
    // columns after C1; this tool reads from whichever the active
    // signature names.
    let active = crate::embed::signature::read_active_signature(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("active embedding signature: {}", e), None)
        })?;
    let col = active.read_column();

    // Per file: current centroid vs centroid of oldest commit chunks touching it.
    let sql = format!(
        "WITH cur AS (
            SELECT f.id AS file_id, f.relative_path AS path,
                   AVG(fc.{col}) AS centroid
            FROM indexed_files f
            JOIN file_chunks fc ON fc.file_id = f.id
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
            GROUP BY f.id, f.relative_path
        ),
        hist AS (
            SELECT f.id AS file_id,
                   AVG(gcc.{col}) AS centroid,
                   COUNT(*)::int AS n
            FROM indexed_files f
            JOIN git_commit_files gcf ON gcf.file_path = f.relative_path
            JOIN git_commits gc ON gc.id = gcf.commit_id
            JOIN git_commit_chunks gcc ON gcc.commit_id = gc.id
            WHERE f.project_id = $1 AND gc.project_id = $1
              AND gcc.{col} IS NOT NULL
              AND gc.author_date < NOW() - INTERVAL '30 days'
            GROUP BY f.id
            HAVING COUNT(*) >= 2
        )
        SELECT cur.path, (cur.centroid <=> hist.centroid)::float8 AS dist, hist.n
        FROM cur JOIN hist ON cur.file_id = hist.file_id
        ORDER BY dist DESC
        LIMIT $2"
    );
    let rows: Vec<(String, f64, i32)> = sqlx::query_as::<_, (String, f64, i32)>(&sql)
        .bind(project_id)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Drift query failed: {}", e), None))?;

    let files: Vec<_> = rows
        .into_iter()
        .map(|(p, d, n)| {
            json!({
                "file": p,
                "cosine_distance": d,
                "historical_chunks": n,
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
        "files": files,
        "guidance": "High cosine distance between current centroid and 30+ day historical centroid = silent semantic change (same name, different behaviour). Hamilton et al. ACL 2016 diachronic-embedding method."
    }))
}
