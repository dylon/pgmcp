//! `tool_work_summary` — deterministic period (monthly) work summary across the
//! git repos in a workspace.
//!
//! Live git is the authoritative source for every fact (commits, line churn,
//! uncommitted state); the temporal-graph index is consulted only as a
//! freshness-gated enrichment. All inputs are validated/clamped/normalized at the
//! request boundary ([`crate::worklog::WorkSummaryRequest::from_params`]) and the
//! resolved parameters are echoed back in the report's `normalized` block — the
//! contract modeled by `docs/formal/tla/WorkSummaryBoundary.tla`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::context::SystemContext;
use crate::mcp::server::WorkSummaryParams;
use crate::mcp::tools::sota_helpers::{json_result, text_result};
use crate::render::ReportFormat;
use crate::worklog::{self, WorkSummaryRequest};

pub async fn tool_work_summary(
    ctx: &SystemContext,
    params: WorkSummaryParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "work_summary", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    // All validation/clamping/normalization happens here, before any work.
    let req = WorkSummaryRequest::from_params(ctx, params)?;
    let report = worklog::summarize(ctx, &req).await?;

    match req.format {
        // The JSON envelope's `normalized` block echoes the resolved params.
        ReportFormat::Json => json_result(&report),
        fmt => Ok(text_result(worklog::report::render(&report, fmt))),
    }
}
