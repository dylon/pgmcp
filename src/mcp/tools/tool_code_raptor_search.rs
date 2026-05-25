//! `tool_code_raptor_search` — query the RAPTOR-over-code summary tree
//! (Sarthi et al. 2024), ported from the memory server's
//! `memory_raptor_search`. (graph-roadmap Phase 3.3)
//!
//! The `code-raptor` cron precomputes one conceptual "module gist" per chunk
//! cluster per project (cluster centroid = its embedding). This tool does
//! cosine ANN over those summaries, answering *conceptual* queries that no
//! single chunk contains ("where does this project handle retry/backoff?",
//! "which module owns auth?"). With `project` omitted it searches every
//! project — the cross-project conceptual lookup.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::CodeRaptorSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_code_raptor_search(
    ctx: &SystemContext,
    params: CodeRaptorSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "code_raptor_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let k = params.k.unwrap_or(10).clamp(1, 100);
    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("no database pool", None))?;

    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;

    let rows = queries::code_raptor_search(pool, &embedding, params.project.as_deref(), k)
        .await
        .map_err(|e| McpError::internal_error(format!("raptor query failed: {}", e), None))?;

    let results: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "project": r.project_name,
                "summary": r.summary_text,
                "similarity": format!("{:.4}", r.similarity),
                "member_count": r.member_count,
                "top_dirs": r.top_topics,
                "sample_files": r.member_paths,
            })
        })
        .collect();

    json_result(&json!({
        "project": params.project.unwrap_or_else(|| "*".to_string()),
        "result_count": results.len(),
        "results": results,
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "guidance": "RAPTOR-over-code summaries: each is a conceptual cluster of related chunks (the \
            cluster centroid is its embedding). Use for conceptual/'where does this project do X' queries \
            that no single chunk answers; omit `project` to compare modules across all indexed projects. \
            Empty results mean the `code-raptor` cron hasn't run yet (needs settled BGE-M3 embeddings). \
            Drill into a summary's `sample_files` with `read_file` / `code_ppr_search`."
    }))
}
