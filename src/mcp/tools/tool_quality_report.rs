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
use crate::quality::aggregate::{DEFAULT_TOOL_TIMEOUT_SECS, aggregate_for_project};
use crate::quality::findings::Severity;
use crate::quality::report::ReportOptions;
use crate::render::ReportFormat;

const DEFAULT_QUALITY_REPORT_TREND_POINTS: usize = 12;
const MAX_QUALITY_REPORT_TREND_POINTS: usize = 120;
const MAX_QUALITY_REPORT_REFRESH_CRONS: usize = 8;

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
    let project = params.project.trim();
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
    let fmt_name = fmt.as_str();
    let min_severity = match params.min_severity.as_deref() {
        None => Severity::Low,
        Some(s) => Severity::parse_floor(s).ok_or_else(|| {
            McpError::invalid_params(
                format!("Unknown min_severity '{s}'. Valid: low|medium|high|critical"),
                None,
            )
        })?,
    };
    let trend_points = params
        .trend_points
        .unwrap_or(DEFAULT_QUALITY_REPORT_TREND_POINTS)
        .min(MAX_QUALITY_REPORT_TREND_POINTS);
    let refresh_jobs = match &params.refresh_crons {
        Some(crons) => {
            if crons.len() > MAX_QUALITY_REPORT_REFRESH_CRONS {
                return Err(McpError::invalid_params(
                    format!(
                        "refresh_crons must contain at most {MAX_QUALITY_REPORT_REFRESH_CRONS} jobs"
                    ),
                    None,
                ));
            }
            let mut jobs = Vec::with_capacity(crons.len());
            for job in crons {
                let job = job.trim();
                if job.is_empty() {
                    return Err(McpError::invalid_params(
                        "refresh_crons entries must be non-empty",
                        None,
                    ));
                }
                jobs.push(job.to_string());
            }
            jobs
        }
        None => Vec::new(),
    };
    let project_id = project_id_or_err(ctx, project).await?;

    debug!(
        tool = "quality_report",
        project = %project,
        format = fmt_name,
        "MCP tool invoked",
    );

    // ── Optional pre-aggregation cron refresh ────────────────────────────
    for job in refresh_jobs {
        crate::mcp::tools::tool_trigger_cron::tool_trigger_cron(
            ctx,
            TriggerCronParams { job, project: None },
        )
        .await?;
    }

    let include_json = params.include_underlying_json.unwrap_or(false);
    let options = ReportOptions {
        include_findings: params.include_findings.unwrap_or(true),
        compute_findings: true,
        include_recommended_fixes: params.include_recommended_fixes.unwrap_or(true),
        min_severity,
        trend_points,
        top_n: 10,
    };

    let mut report =
        aggregate_for_project(ctx, project_id, project, options, DEFAULT_TOOL_TIMEOUT_SECS).await?;

    // ── Persist GPA history (best-effort; never fatal) ───────────────────
    if let Some(pool) = ctx.db().pool() {
        crate::quality::history::insert_history(pool, project_id, &report).await;
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
            "format": fmt_name,
            "report": crate::render::report_json_value(&report),
        });
        let text = serde_json::to_string_pretty(&envelope)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
        return Ok(CallToolResult::success(vec![Content::text(text)]));
    }

    Ok(CallToolResult::success(vec![Content::text(rendered)]))
}
