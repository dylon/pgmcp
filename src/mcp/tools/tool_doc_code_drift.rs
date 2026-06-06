//! `tool_doc_code_drift` — Cosine distance between doc-chunk and code-chunk
//! embeddings per file pair (SOTA Phase 4.4).
//!
//! Per file: compute a doc centroid (markdown-language chunks within the file's
//! directory) and a code centroid (rust/python/etc.) and report the cosine
//! distance. High = docs and code diverged in vocabulary.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DocCodeDriftParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

const DEFAULT_MIN_DRIFT: f64 = 0.3;
const MAX_COSINE_DISTANCE: f64 = 2.0;
const DEFAULT_LIMIT: i32 = 30;
const MAX_LIMIT: i32 = 100;

fn normalize_min_drift(value: Option<f64>) -> Result<f64, McpError> {
    let value = value.unwrap_or(DEFAULT_MIN_DRIFT);
    if !value.is_finite() {
        return Err(McpError::invalid_params(
            "min_drift must be a finite number",
            None,
        ));
    }
    Ok(value.clamp(0.0, MAX_COSINE_DISTANCE))
}

pub async fn tool_doc_code_drift(
    ctx: &SystemContext,
    params: DocCodeDriftParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "doc_code_drift", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;
    let min_drift = normalize_min_drift(params.min_drift)?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(0, MAX_LIMIT);

    // Phase 5 C7: signature-aware column resolution.
    let active = crate::embed::signature::read_active_signature(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("active embedding signature: {}", e), None)
        })?;
    let col = active.read_column();

    // For every directory in the project, compute a single drift score:
    // cosine(distance) between the centroid of markdown chunks and the centroid
    // of non-markdown chunks. Uses pgvector's `<=>` operator (cosine distance).
    let sql = format!(
        "WITH dir_emb AS (
            SELECT
                CASE
                    WHEN position('/' IN reverse(f.relative_path)) > 0
                    THEN substring(f.relative_path FROM 1 FOR length(f.relative_path) - position('/' IN reverse(f.relative_path)))
                    ELSE ''
                END AS dir,
                CASE WHEN f.language = 'markdown' THEN 'doc' ELSE 'code' END AS kind,
                AVG(fc.{col}) AS centroid,
                COUNT(*)::int AS n
            FROM indexed_files f
            JOIN file_chunks fc ON fc.file_id = f.id
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
            GROUP BY dir, kind
        ),
        paired AS (
            SELECT d.dir,
                   d.centroid AS doc_centroid,
                   c.centroid AS code_centroid,
                   d.n AS doc_chunks,
                   c.n AS code_chunks
            FROM dir_emb d JOIN dir_emb c
                ON d.dir = c.dir AND d.kind = 'doc' AND c.kind = 'code'
        )
        SELECT dir, dist, doc_chunks, code_chunks
        FROM (
            SELECT dir,
                   (doc_centroid <=> code_centroid)::float8 AS dist,
                   doc_chunks,
                   code_chunks
            FROM paired
        ) scored
        WHERE dist >= $2
        ORDER BY dist DESC
        LIMIT $3"
    );
    let rows: Vec<(String, f64, i32, i32)> = sqlx::query_as::<_, (String, f64, i32, i32)>(&sql)
        .bind(project_id)
        .bind(min_drift)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Drift query failed: {}", e), None))?;

    let out: Vec<_> = rows
        .into_iter()
        .map(|(dir, dist, dc, cc)| {
            json!({
                "directory": dir,
                "cosine_distance": dist,
                "doc_chunks": dc,
                "code_chunks": cc,
            })
        })
        .collect();

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "min_drift": min_drift,
        "limit": limit,
        "directories": out,
        "guidance": "Higher cosine distance = doc and code drift apart in vocabulary, suggesting stale documentation."
    }))
}
