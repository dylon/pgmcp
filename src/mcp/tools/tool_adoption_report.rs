//! `adoption_report` — measure tool-family adoption from `mcp_tool_calls`.
//!
//! Reads the durable telemetry table directly (an independent collector; it
//! does not touch `mcp_tool_telemetry`) and reports per-family call share and
//! session adoption by client, restricted to the real-client allowlist. This is
//! the baseline/lift instrument for the social-tool adoption work.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_adoption_report(
    ctx: &SystemContext,
    params: AdoptionReportParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("adoption_report requires a real PgPool", None))?;

    // Default 30-day window; cap at 31 days (same bound as mcp_tool_telemetry).
    let window_minutes = params.since_minutes.unwrap_or(43_200).clamp(1, 44_640) as i64;

    let report = crate::adoption::collect(pool, window_minutes)
        .await
        .map_err(|e| McpError::internal_error(format!("adoption query failed: {e}"), None))?;

    let body = match params.format.as_deref().unwrap_or("json") {
        "markdown" | "md" => report.to_markdown(),
        "json" => serde_json::to_string_pretty(&report.to_json())
            .unwrap_or_else(|_| report.to_json().to_string()),
        other => {
            return Err(McpError::invalid_params(
                format!("Unknown format '{other}': expected json | markdown"),
                None,
            ));
        }
    };

    Ok(CallToolResult::success(vec![Content::text(body)]))
}
