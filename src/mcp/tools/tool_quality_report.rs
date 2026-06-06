//! `tool_quality_report` — aggregate every analysis finding into a graded,
//! three-pillar report and render it (Markdown / Org / LaTeX / HTML / text /
//! JSON). Thin wrapper: validate params, optionally refresh stale crons,
//! aggregate, persist GPA history, render.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::{QualityReportParams, TriggerCronParams};
use crate::mcp::tools::sota_helpers::project_id_or_err;
use crate::quality::aggregate::{DEFAULT_TOOL_TIMEOUT_SECS, aggregate};
use crate::quality::findings::Severity;
use crate::quality::report::ReportOptions;
use crate::render::ReportFormat;

pub async fn tool_quality_report(
    ctx: &SystemContext,
    params: QualityReportParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .quality_report_scans
        .fetch_add(1, Ordering::Relaxed);

    // ── Validate format + min_severity (error, don't silently default) ───
    let fmt_str = params.format.as_deref().unwrap_or("markdown");
    let fmt = ReportFormat::parse(fmt_str).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "Unknown format '{fmt_str}'. Valid: {}",
                ReportFormat::valid_values()
            ),
            None,
        )
    })?;
    let min_severity = match params.min_severity.as_deref() {
        None => Severity::Low,
        Some(s) => Severity::parse_floor(s).ok_or_else(|| {
            McpError::invalid_params(
                format!("Unknown min_severity '{s}'. Valid: low|medium|high|critical"),
                None,
            )
        })?,
    };

    debug!(
        tool = "quality_report",
        project = %params.project,
        format = fmt_str,
        "MCP tool invoked",
    );

    // ── Optional pre-aggregation cron refresh ────────────────────────────
    if let Some(crons) = &params.refresh_crons {
        for job in crons {
            crate::mcp::tools::tool_trigger_cron::tool_trigger_cron(
                ctx,
                TriggerCronParams {
                    job: job.clone(),
                    project: None,
                },
            )
            .await?;
        }
    }

    let include_json = params.include_underlying_json.unwrap_or(false);
    let options = ReportOptions {
        include_findings: params.include_findings.unwrap_or(true),
        compute_findings: true,
        include_recommended_fixes: params.include_recommended_fixes.unwrap_or(true),
        min_severity,
        trend_points: params.trend_points.unwrap_or(12),
        top_n: 10,
    };

    let mut report = aggregate(ctx, &params.project, options, DEFAULT_TOOL_TIMEOUT_SECS).await?;

    // ── Persist GPA history (best-effort; never fatal) ───────────────────
    if let Some(pool) = ctx.db().pool()
        && let Ok(pid) = project_id_or_err(ctx, &params.project).await
    {
        crate::quality::history::insert_history(pool, pid, &report).await;
    }

    // Drop per-finding raw payloads unless a JSON consumer asked for them
    // (keeps the default render path lean over thousands of findings).
    let raw_wanted = include_json || fmt == ReportFormat::Json;
    if !raw_wanted {
        for f in &mut report.findings {
            f.raw = None;
        }
    }

    let rendered = crate::render::render(&report, fmt);

    debug!(
        tool = "quality_report",
        findings = report.findings.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    if include_json {
        let envelope = json!({
            "rendered": rendered,
            "format": fmt_str,
            "report": crate::render::report_json_value(&report),
        });
        let text = serde_json::to_string_pretty(&envelope)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
        return Ok(CallToolResult::success(vec![Content::text(text)]));
    }

    Ok(CallToolResult::success(vec![Content::text(rendered)]))
}
