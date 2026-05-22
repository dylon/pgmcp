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
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DocCodeDriftParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_doc_code_drift(
    ctx: &SystemContext,
    params: DocCodeDriftParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "doc_code_drift", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    // For every directory in the project, compute a single drift score:
    // cosine(distance) between the centroid of markdown chunks and the centroid
    // of non-markdown chunks. Uses pgvector's `<=>` operator (cosine distance).
    let rows: Vec<(String, f64, i32, i32)> = sqlx::query_as::<_, (String, f64, i32, i32)>(
        "WITH dir_emb AS (
            SELECT
                CASE
                    WHEN position('/' IN reverse(f.relative_path)) > 0
                    THEN substring(f.relative_path FROM 1 FOR length(f.relative_path) - position('/' IN reverse(f.relative_path)))
                    ELSE ''
                END AS dir,
                CASE WHEN f.language = 'markdown' THEN 'doc' ELSE 'code' END AS kind,
                AVG(fc.embedding) AS centroid,
                COUNT(*)::int AS n
            FROM indexed_files f
            JOIN file_chunks fc ON fc.file_id = f.id
            WHERE f.project_id = $1
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
        SELECT dir, (doc_centroid <=> code_centroid)::float8 AS dist, doc_chunks, code_chunks
        FROM paired
        ORDER BY dist DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Drift query failed: {}", e), None))?;

    let min_drift = params.min_drift.unwrap_or(0.3);
    let limit = params.limit.unwrap_or(30);

    let mut out: Vec<_> = rows
        .into_iter()
        .filter(|(_d, dist, _, _)| *dist >= min_drift)
        .map(|(dir, dist, dc, cc)| {
            json!({
                "directory": dir,
                "cosine_distance": dist,
                "doc_chunks": dc,
                "code_chunks": cc,
            })
        })
        .collect();
    out.truncate(limit.max(0) as usize);

    json_result(&json!({
        "project": params.project,
        "min_drift": min_drift,
        "directories": out,
        "guidance": "Higher cosine distance = doc and code drift apart in vocabulary, suggesting stale documentation."
    }))
}
