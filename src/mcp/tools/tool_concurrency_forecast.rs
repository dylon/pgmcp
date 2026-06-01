//! `tool_concurrency_forecast` — OLS forecast of a concurrency-health metric.
//!
//! Mirrors `tool_quality_forecast`: fit an OLS slope over a metric series in
//! `concurrency_health_history` (filled by the `concurrency-scan` cron) and
//! answer "on this trajectory, deadlock cycles / lock contention hit the
//! threshold in N weeks". Read-only; reuses the pure
//! `src/quality/forecast.rs::{ols_slope, weeks_to_threshold, pct_change}` so the
//! number agrees with the digest's concurrency-trend line. Short/empty history
//! degrades gracefully to a `null` forecast with an explanatory note.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::ConcurrencyForecastParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::quality::forecast::{ols_slope, pct_change, weeks_to_threshold};

const ALLOWED_METRICS: &[&str] = &[
    "deadlock_cycle_count",
    "channel_cycle_count",
    "blocked_recv_count",
    "max_lock_contention",
];

pub async fn tool_concurrency_forecast(
    ctx: &SystemContext,
    params: ConcurrencyForecastParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "concurrency_forecast", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let days = params.days.unwrap_or(90).clamp(1, 3650);
    let metric = params.metric.as_deref().unwrap_or("deadlock_cycle_count");
    if !ALLOWED_METRICS.contains(&metric) {
        return Err(McpError::invalid_params(
            format!("metric must be one of {ALLOWED_METRICS:?}"),
            None,
        ));
    }
    // Default threshold: a "this is a problem" floor per metric.
    let threshold = params.threshold.unwrap_or(match metric {
        "max_lock_contention" => 10.0,
        _ => 5.0,
    });

    let series = queries::concurrency_metric_series(pool, project_id, metric, days)
        .await
        .map_err(|e| McpError::internal_error(format!("forecast series failed: {e}"), None))?;

    // Points with x increasing forward in time (x = -days_ago); y = value. So a
    // positive slope means the metric is RISING (worse). `series` is oldest→newest.
    let points: Vec<(f64, f64)> = series.iter().map(|(days_ago, v)| (-days_ago, *v)).collect();
    let current = series.last().map(|(_, v)| *v);
    let prev = (series.len() >= 2).then(|| series[series.len() - 2].1);
    let slope_per_day = ols_slope(&points);
    let slope_per_week = slope_per_day.map(|s| s * 7.0);
    let pct = match (prev, current) {
        (Some(p), Some(c)) => pct_change(p, c),
        _ => None,
    };
    let weeks = match (current, slope_per_day) {
        (Some(c), Some(s)) => weeks_to_threshold(c, s, threshold),
        _ => None,
    };

    let note = if series.len() < 2 {
        Some("insufficient history: fewer than two concurrency-health snapshots in the window — run the concurrency-scan cron (or wait a tick) to accumulate a trajectory".to_string())
    } else if slope_per_day.is_none() {
        Some("no finite slope: the snapshots share a single timestamp".to_string())
    } else if weeks.is_none() {
        Some(format!(
            "trajectory does not cross {threshold:.2}: `{metric}` is flat, falling, or already at/past the threshold"
        ))
    } else {
        None
    };

    json_result(&json!({
        "project": params.project,
        "metric": metric,
        "window_days": days,
        "sample_count": series.len(),
        "current": current,
        "pct_change": pct,
        "slope_per_day": slope_per_day,
        "slope_per_week": slope_per_week,
        "weeks_to_threshold": weeks,
        "threshold": threshold,
        "note": note,
        "guidance": "Forecasts a concurrency-health metric over `concurrency_health_history` (filled \
            by the concurrency-scan cron) via OLS. Positive slope = rising (worse); \
            weeks_to_threshold is the ETA to the threshold on the current trajectory. Reuses the \
            same forecast math as quality_forecast.",
    }))
}
