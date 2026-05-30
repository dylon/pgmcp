//! `tool_quality_trend` — MCP tool body (Phase 1: trends & forecasting).
//!
//! Turns the `quality_report_history` snapshots (populated by the
//! `quality-history` cron) into a *trajectory*: the per-pillar GPA series
//! (Engineering / Architecture / Security / overall) over the requested
//! window, an EWMA-smoothed overall line (so a single stale-cron run does not
//! render as a spike — the same [`PillarTrend::ewma`] the `quality_report`
//! trend strip uses), and the first→last delta of each pillar.
//!
//! The math is read-only: it reuses [`crate::quality::history::gpa_series_since`]
//! and shares no query logic of its own. The companion `quality_forecast` tool
//! (`tool_quality_forecast.rs`) projects the slope forward to a threshold.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::QualityTrendParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::quality::findings::Pillar;
use crate::quality::history::{GpaPoint, gpa_series_since};
use crate::quality::report::PillarTrend;

/// First→last delta of a series of optional GPAs, ignoring `None` (N/A) points.
/// Returns `{first, last, delta}` JSON, or `null` if fewer than two real points.
fn pillar_delta(values: &[Option<f32>]) -> serde_json::Value {
    let present: Vec<f64> = values.iter().filter_map(|v| v.map(|x| x as f64)).collect();
    match present.as_slice() {
        [] | [_] => serde_json::Value::Null,
        [first, .., last] => json!({
            "first": first,
            "last": last,
            "delta": last - first,
        }),
    }
}

/// Build a `PillarTrend` over the non-`None` values of one pillar column so we
/// can reuse its `ewma` smoother. The pillar tag is cosmetic here (the trend is
/// only used for its smoothing), so any pillar value is fine.
fn ewma_of(pillar: Pillar, values: &[Option<f32>], span: usize) -> Vec<f64> {
    let gpas: Vec<f64> = values.iter().filter_map(|v| v.map(|x| x as f64)).collect();
    PillarTrend { pillar, gpas }.ewma(span)
}

pub async fn tool_quality_trend(
    ctx: &SystemContext,
    params: QualityTrendParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let days = params.days.unwrap_or(90);

    let series: Vec<GpaPoint> = gpa_series_since(pool, project_id, days).await;

    // Per-sample rows: the timestamp axis + all four GPA tracks. Preallocated to
    // the series length (one row per snapshot).
    let mut points = Vec::with_capacity(series.len());
    for p in &series {
        points.push(json!({
            "at": p.at.to_rfc3339(),
            "engineering": p.engineering,
            "architecture": p.architecture,
            "security": p.security,
            "overall": p.overall,
        }));
    }

    // Columnar views for the EWMA smoother and the delta column.
    let eng: Vec<Option<f32>> = series.iter().map(|p| p.engineering).collect();
    let arch: Vec<Option<f32>> = series.iter().map(|p| p.architecture).collect();
    let sec: Vec<Option<f32>> = series.iter().map(|p| p.security).collect();
    let overall: Vec<Option<f32>> = series.iter().map(|p| p.overall).collect();

    // 3-point EWMA (span 3 → alpha 0.5), matching the report trend strip.
    const EWMA_SPAN: usize = 3;
    let overall_ewma = ewma_of(Pillar::Engineering, &overall, EWMA_SPAN);

    let note = if series.len() < 2 {
        Some(
            "insufficient history: fewer than two quality snapshots in the window — run the quality-history cron (or wait a tick) to accumulate a trajectory",
        )
    } else {
        None
    };

    json_result(&json!({
        "project": params.project,
        "window_days": days.clamp(1, 3650),
        "sample_count": series.len(),
        "points": points,
        "overall_ewma": overall_ewma,
        "delta": {
            "engineering": pillar_delta(&eng),
            "architecture": pillar_delta(&arch),
            "security": pillar_delta(&sec),
            "overall": pillar_delta(&overall),
        },
        "note": note,
    }))
}
