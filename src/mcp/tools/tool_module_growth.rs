//! `tool_module_growth` — project- or file-level growth trajectory over time.
//!
//! Buckets `git_commits` (joined to `git_commit_files` when a single file is
//! requested) into week / month / quarter periods and aggregates commits +
//! distinct authors. Linear-regression slope on the last `lookback_buckets`
//! gives a simple projection.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::Utc;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_module_growth(
    ctx: &SystemContext,
    params: ModuleGrowthParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .growth_trajectory_scans
        .fetch_add(1, Ordering::Relaxed);

    let bucket = params.bucket.as_deref().unwrap_or("month");
    let bucket = match bucket {
        "week" | "month" | "quarter" => bucket,
        _ => "month",
    };
    let lookback_buckets = params.lookback_buckets.unwrap_or(12).max(1);

    debug!(
        tool = "module_growth_trajectory",
        project = %params.project,
        file = params.file.as_deref().unwrap_or("*"),
        bucket,
        lookback_buckets,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let buckets = queries::get_growth_buckets(
        pool,
        &params.project,
        params.file.as_deref(),
        bucket,
        lookback_buckets,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Growth query failed: {}", e), None))?;

    if buckets.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "scope": if params.file.is_some() { "file" } else { "project" },
                "path": params.file,
                "buckets": [],
                "trend": null,
                "guidance": "No commit history within the lookback window. Either git history is \
                             disabled, or the project has no recent activity.",
                "health": json!({"git_history_present": false}),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Linear-regression slope of commits_per_bucket over time index.
    let n = buckets.len() as f64;
    let xs: Vec<f64> = (0..buckets.len()).map(|i| i as f64).collect();
    let ys: Vec<f64> = buckets.iter().map(|b| b.commits as f64).collect();
    let mean_x: f64 = xs.iter().sum::<f64>() / n;
    let mean_y: f64 = ys.iter().sum::<f64>() / n;
    let num: f64 = xs
        .iter()
        .zip(ys.iter())
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let den: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    let slope = if den.abs() < 1e-9 { 0.0 } else { num / den };
    let predicted_next = (mean_y + slope * (n - 1.0 + 1.0)).max(0.0);

    let buckets_json: Vec<serde_json::Value> = buckets
        .iter()
        .map(|b| {
            json!({
                "period_start": b.period_start.to_rfc3339(),
                "commits": b.commits,
                "authors": b.authors,
                "additions": b.additions,
                "deletions": b.deletions,
            })
        })
        .collect();

    let trend = if buckets.len() >= 4 {
        json!({
            "slope_commits_per_bucket": format!("{:.3}", slope),
            "mean_commits_per_bucket": format!("{:.2}", mean_y),
            "predicted_commits_next_bucket": predicted_next.round() as i64,
        })
    } else {
        json!(null)
    };

    let recommendation = if slope > 1.5 && buckets.len() >= 4 {
        "Activity is accelerating. If this is a single file, consider preemptive split before it \
         becomes a god module. For project-scope, monitor for hot-path emergence."
    } else if slope < -0.5 && buckets.len() >= 4 {
        "Activity is decelerating. The module may be stabilizing — or going stale; cross-check \
         with `stale_zombie_detector`."
    } else {
        "Activity is stable. No action required from a growth-trajectory perspective."
    };

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    let result = json!({
        "effect_breakdown": effect_breakdown,
        "scope": if params.file.is_some() { "file" } else { "project" },
        "path": params.file,
        "bucket": bucket,
        "buckets": buckets_json,
        "trend": trend,
        "recommendation": recommendation,
        "health": json!({"git_history_present": true}),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "module_growth_trajectory",
        bucket_count = buckets.len(),
        slope,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}
