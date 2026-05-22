//! `tool_commit_changepoint` — Page CUSUM change-point on per-file commit rate
//! (SOTA Phase 11.2).

#![allow(unused_imports)]

use chrono::{DateTime, Datelike, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CommitChangepointParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_commit_changepoint(
    ctx: &SystemContext,
    params: CommitChangepointParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "commit_changepoint", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(20);

    let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "SELECT gcf.file_path, gc.committed_at
         FROM git_commits gc
         JOIN git_commit_files gcf ON gcf.commit_id = gc.id
         WHERE gc.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Commit query failed: {}", e), None))?;

    // Bucket commits per file by ISO week.
    let mut per_file: std::collections::HashMap<String, BTreeMap<(i32, u32), u32>> =
        std::collections::HashMap::new();
    for (path, dt) in rows {
        let iso = dt.iso_week();
        let key = (iso.year(), iso.week());
        *per_file.entry(path).or_default().entry(key).or_insert(0) += 1;
    }

    let mut changepoints: Vec<(String, String, f64)> = Vec::new();
    for (path, weekly) in per_file {
        if weekly.len() < 4 {
            continue;
        }
        let series: Vec<u32> = weekly.values().copied().collect();
        let mean: f64 = series.iter().map(|x| *x as f64).sum::<f64>() / series.len() as f64;
        let var: f64 = series
            .iter()
            .map(|x| {
                let d = *x as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / series.len() as f64;
        let stddev = var.sqrt().max(0.5);
        let k = 0.5 * stddev;
        let h = 4.0 * stddev;
        let mut s_pos = 0.0;
        let mut best_idx: Option<usize> = None;
        let mut best_score = 0.0;
        for (i, &x) in series.iter().enumerate() {
            s_pos = (s_pos + (x as f64 - mean - k)).max(0.0);
            if s_pos > best_score {
                best_score = s_pos;
                best_idx = Some(i);
            }
        }
        if best_score >= h
            && let Some(i) = best_idx
        {
            let weeks_sorted: Vec<&(i32, u32)> = weekly.keys().collect();
            if let Some((y, w)) = weeks_sorted.get(i) {
                changepoints.push((path, format!("{}-W{:02}", y, w), best_score));
            }
        }
    }
    changepoints.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    changepoints.truncate(limit.max(0) as usize);
    let rows_json: Vec<_> = changepoints
        .iter()
        .map(|(p, w, s)| json!({"file": p, "changepoint_week": w, "cusum_score": s}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "changepoints": rows_json,
        "guidance": "Page CUSUM detects regime shifts in per-file commit rate. A flagged file transitioned from stable to unstable (or vice versa) at the reported week."
    }))
}
