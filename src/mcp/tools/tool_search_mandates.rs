//! `tool_search_mandates` — Phase 0 memory-server quick win.
//!
//! Adds a search surface for `durable_mandates`, which previously had a
//! single reader (`list_durable_mandates_for_project`, a project-scope
//! dump with no filtering or ranking). Phase 0 ships Postgres FTS over
//! `imperative || target`; the same tool gains a semantic mode after
//! Phase 1 cutover provisions a 1024d BGE-M3 embedding column.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tracing::{debug, error, info};

use crate::context::SystemContext;
use crate::mcp::server::SearchMandatesParams;

const VALID_POLARITIES: &[&str] = &[
    "always",
    "never",
    "prefer",
    "avoid",
    "remember",
    "from_now_on",
    "correction",
    "permission",
    "constraint",
    "mandate",
    "process_rule",
    "project_rule",
];

pub async fn tool_search_mandates(
    ctx: &SystemContext,
    params: SearchMandatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_search_mandates
        .fetch_add(1, Ordering::Relaxed);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))?;

    let limit = params.limit.unwrap_or(20).clamp(1, 200);
    if params.query.trim().is_empty() {
        return Err(McpError::invalid_params("query must not be empty", None));
    }
    if let Some(p) = params.polarity.as_deref()
        && !VALID_POLARITIES.contains(&p)
    {
        return Err(McpError::invalid_params(
            format!(
                "invalid polarity '{}'; must be one of {:?}",
                p, VALID_POLARITIES
            ),
            None,
        ));
    }
    if let Some(s) = params.scope.as_deref()
        && !matches!(s, "project" | "workspace")
    {
        return Err(McpError::invalid_params(
            "scope must be 'project' or 'workspace'",
            None,
        ));
    }

    debug!(
        tool = "search_mandates",
        query = %truncate(&params.query, 200),
        polarity = params.polarity.as_deref().unwrap_or("*"),
        scope = params.scope.as_deref().unwrap_or("*"),
        project_id = params.project_id.unwrap_or(-1),
        limit,
        "MCP tool invoked",
    );

    let results = crate::db::queries::search_mandates_fts(
        pool,
        &params.query,
        params.polarity.as_deref(),
        params.scope.as_deref(),
        params.project_id,
        limit,
    )
    .await
    .map_err(|e| {
        error!(tool = "search_mandates", error = %e, "query failed");
        McpError::internal_error(format!("query failed: {}", e), None)
    })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "count": count,
        "mode": "fts",
        "results": results,
    }))
    .map_err(|e| McpError::internal_error(format!("serialization failed: {}", e), None))?;

    debug!(
        tool = "search_mandates",
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
