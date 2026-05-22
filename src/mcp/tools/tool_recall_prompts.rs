//! `tool_recall_prompts` — Phase 0 memory-server quick win.
//!
//! Surfaces the existing `session_prompts.embedding` column via vector
//! similarity search. The column has been populated on every prompt since
//! the session-mandates feature shipped, but no read path existed before
//! this — see `docs/memory-server/00-context-and-gap.md` §5.5.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::mcp::server::RecallPromptsParams;

pub async fn tool_recall_prompts(
    ctx: &SystemContext,
    params: RecallPromptsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_recall_prompts
        .fetch_add(1, Ordering::Relaxed);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))?;

    let limit = params.limit.unwrap_or(10).clamp(1, 200);
    let session_id =
        match params.session.as_deref() {
            Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
                McpError::invalid_params(format!("invalid session UUID: {}", e), None)
            })?),
            None => None,
        };

    debug!(
        tool = "recall_prompts",
        query = %truncate(&params.query, 200),
        limit,
        project = params.project.as_deref().unwrap_or("*"),
        session = session_id.map(|u| u.to_string()).unwrap_or_else(|| "*".into()),
        "MCP tool invoked",
    );

    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "recall_prompts", error = %e, "embedding failed");
        McpError::internal_error(format!("embedding failed: {}", e), None)
    })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let results = crate::db::queries::recall_prompts_semantic(
        pool,
        &embedding,
        params.project.as_deref(),
        session_id,
        limit,
        ef_search,
    )
    .await
    .map_err(|e| {
        error!(tool = "recall_prompts", error = %e, "query failed");
        McpError::internal_error(format!("query failed: {}", e), None)
    })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "count": count,
        "results": results,
    }))
    .map_err(|e| McpError::internal_error(format!("serialization failed: {}", e), None))?;

    debug!(
        tool = "recall_prompts",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        json,
    )]))
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &s[..end]
}
