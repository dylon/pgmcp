//! `tool_effect_drift` — temporal effect-drift query over the
//! `symbol_effect_history` ledger (v15).
//!
//! "Which functions recently became `unsafe` / `async` / `blocking_io`?",
//! "what effects has this project shed?" — answered from the append-only
//! ledger the symbol-extraction cron maintains by diffing each file's
//! freshly-extracted effect set against the prior one. Newest-first, with
//! optional project / effect / change / recency filters.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::EffectDriftParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_effect_drift(
    ctx: &SystemContext,
    params: EffectDriftParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "effect_drift", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(50).clamp(1, 500) as i64;
    let change = match params.change.as_deref() {
        None => None,
        Some(c @ ("gained" | "lost")) => Some(c),
        Some(other) => {
            return Err(McpError::invalid_params(
                format!("change must be 'gained' or 'lost', got '{other}'"),
                None,
            ));
        }
    };
    // Recency window: only transitions observed within the last N days.
    let since = params
        .since_days
        .filter(|d| *d > 0)
        .map(|d| chrono::Utc::now() - chrono::Duration::days(d));

    let rows = crate::db::queries::query_effect_drift(
        pool,
        params.project.as_deref(),
        params.effect.as_deref(),
        change,
        since,
        limit,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("effect_drift query failed: {e}"), None))?;

    let gained = rows.iter().filter(|r| r.change == "gained").count();
    let lost = rows.iter().filter(|r| r.change == "lost").count();
    json_result(&json!({
        "count": rows.len(),
        "gained": gained,
        "lost": lost,
        "drift": rows,
    }))
}
