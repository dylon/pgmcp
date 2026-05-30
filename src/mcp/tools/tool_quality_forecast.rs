//! `tool_quality_forecast` — MCP tool body (Phase 1: trends & forecasting).
//!
//! The "red-date" projection: fit an OLS slope over the overall-GPA series in
//! `quality_report_history` and answer "on this trajectory, the overall GPA
//! hits the C-grade floor (2.0) in N weeks". Read-only — it reuses
//! [`crate::quality::history::gpa_series_since`] /
//! [`crate::quality::history::overall_gpa_slope_per_day`] and the pure
//! [`crate::quality::forecast::weeks_to_threshold`] so the number agrees with
//! the digest's "GPA trending …" line.
//!
//! Short/empty history degrades gracefully: a flat or rising trajectory (or
//! one already at/past the threshold) yields a `null` `weeks_to_threshold`
//! with an explanatory note, never an error.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::QualityForecastParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::quality::forecast::weeks_to_threshold;
use crate::quality::history::{GpaPoint, gpa_series_since, overall_gpa_slope_per_day};

/// C-grade floor on the 4-point GPA scale — the default threshold the forecast
/// projects the crossing of ("when does quality fall to a C?").
const C_GRADE_FLOOR: f64 = 2.0;

pub async fn tool_quality_forecast(
    ctx: &SystemContext,
    params: QualityForecastParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let days = params.days.unwrap_or(90);
    let threshold = params.threshold.unwrap_or(C_GRADE_FLOOR);

    let series: Vec<GpaPoint> = gpa_series_since(pool, project_id, days).await;

    // Most-recent overall GPA actually present in the window (the series is
    // oldest → newest, so scan from the back for the last non-null).
    let current_overall: Option<f64> = series
        .iter()
        .rev()
        .find_map(|p| p.overall.map(|g| g as f64));

    // Per-day slope (None when fewer than two overall points exist).
    let slope_per_day = overall_gpa_slope_per_day(&series);
    let slope_per_week = slope_per_day.map(|s| s * 7.0);

    // Weeks until the current GPA, on this slope, reaches the threshold. None
    // when flat / diverging / already past, or when we lack a slope or a
    // current value.
    let weeks = match (current_overall, slope_per_day) {
        (Some(latest), Some(slope)) => weeks_to_threshold(latest, slope, threshold),
        _ => None,
    };

    // A single, honest explanation of why the forecast may be empty.
    let note = if series.len() < 2 {
        Some("insufficient history: fewer than two overall-GPA snapshots in the window — run the quality-history cron (or wait a tick) to accumulate a trajectory".to_string())
    } else if slope_per_day.is_none() {
        Some("no finite slope: the overall-GPA points share a single timestamp".to_string())
    } else if weeks.is_none() {
        Some(format!(
            "trajectory does not cross {threshold:.2}: the overall GPA is flat, improving, or already at/past the threshold"
        ))
    } else {
        None
    };

    json_result(&json!({
        "project": params.project,
        "window_days": days.clamp(1, 3650),
        "sample_count": series.len(),
        "current_overall": current_overall,
        "slope_per_day": slope_per_day,
        "slope_per_week": slope_per_week,
        "weeks_to_threshold": weeks,
        "threshold": threshold,
        "note": note,
    }))
}
