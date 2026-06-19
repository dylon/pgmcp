//! `tool_bus_factor_map` — knowledge-concentration risk per file.
//!
//! For each file in the top half of pagerank (configurable), report:
//! - top blame author + their share of blamed lines
//! - distinct author count
//! - last touch date
//! - risk_score = pagerank × top_share / max(1, distinct_authors)
//!
//! Plus a project-level `bus_factor_estimate` = the minimum number of
//! authors needed to cover ≥50% of total blamed lines (greedy set cover).

#![allow(unused_imports)]

use std::collections::HashMap;
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

pub async fn tool_bus_factor_map(
    ctx: &SystemContext,
    params: BusFactorMapParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().bus_factor_scans.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(30).max(1);
    let min_pagerank_pct = params
        .min_pagerank_percentile
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);

    debug!(
        tool = "bus_factor_map",
        project = %params.project,
        min_pagerank_pct,
        limit,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let rows = queries::find_bus_factor_files(pool, &params.project, min_pagerank_pct, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("Bus-factor query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "critical": [],
                "warning": [],
                "healthy_count": 0,
                "bus_factor_estimate": 0,
                "parameters": {
                    "project": params.project,
                    "min_pagerank_percentile": min_pagerank_pct,
                    "limit": limit,
                },
                "guidance": "No bus-factor data. Either blame columns are empty (run git-blame \
                             cron) or all qualifying files are below the pagerank percentile.",
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    let graph_present = rows.iter().any(|r| r.pagerank.is_some());

    // Estimate the project bus factor via greedy author cover ≥50% of total
    // blamed lines. Approximation: aggregate top-author share by author over
    // the rows we have.
    let mut author_to_score: HashMap<String, f64> = HashMap::new();
    for r in &rows {
        let weight = r.top_share * r.pagerank.unwrap_or(1.0);
        *author_to_score.entry(r.top_author.clone()).or_insert(0.0) += weight;
    }
    let mut sorted: Vec<(String, f64)> = author_to_score.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let total: f64 = sorted.iter().map(|(_, s)| s).sum();
    let half = total / 2.0;
    let mut accum = 0.0;
    let mut bus_factor_estimate = 0_u32;
    for (_, s) in &sorted {
        bus_factor_estimate += 1;
        accum += s;
        if accum >= half {
            break;
        }
    }
    if total <= 0.0 {
        bus_factor_estimate = 0;
    }

    let mut critical: Vec<serde_json::Value> = Vec::new();
    let mut warning: Vec<serde_json::Value> = Vec::new();
    let mut healthy_count = 0_u64;

    for r in &rows {
        let last_touch_days = r.last_touch.map(|t| {
            let days = (Utc::now() - t).num_days();
            days.max(0)
        });
        let risk = r.risk_score.unwrap_or(0.0);
        let row = json!({
            "path": r.relative_path,
            "top_author": r.top_author,
            "top_author_share": format!("{:.4}", r.top_share),
            "distinct_authors": r.distinct_authors,
            "last_touch_days": last_touch_days,
            "pagerank": r.pagerank,
            "risk_score": format!("{:.6}", risk),
            "recommendation": pick_recommendation(r.distinct_authors, r.top_share, last_touch_days),
        });
        // Bucket: top author owns >=70% AND distinct_authors <= 2 → critical
        // top author owns >=50% AND distinct_authors <= 3 → warning
        // else healthy
        if r.top_share >= 0.70 && r.distinct_authors <= 2 {
            critical.push(row);
        } else if r.top_share >= 0.50 && r.distinct_authors <= 3 {
            warning.push(row);
        } else {
            healthy_count += 1;
        }
    }

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
        "critical": critical,
        "warning": warning,
        "healthy_count": healthy_count,
        "bus_factor_estimate": bus_factor_estimate,
        "parameters": {
            "project": params.project,
            "min_pagerank_percentile": min_pagerank_pct,
            "limit": limit,
        },
        "guidance": format!(
            "Bus-factor estimate: {} author(s) needed to cover ≥50% of project knowledge \
             (weighted by pagerank × top_share). Critical files have a single author owning \
             ≥70% of lines; warning files have 50–70% concentration. Recommendations propose \
             pair-programming, docs, or knowledge-transfer sessions.",
            bus_factor_estimate
        ),
        "health": health_envelope(graph_present, true),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "bus_factor_map",
        rows = rows.len(),
        bus_factor = bus_factor_estimate,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn pick_recommendation(
    distinct_authors: i64,
    top_share: f64,
    last_touch_days: Option<i64>,
) -> String {
    let stale = last_touch_days.is_some_and(|d| d > 365);
    if distinct_authors == 1 && stale {
        "Single owner who hasn't touched the file in over a year. Confirm continuity or document \
         and add tests before further changes."
            .to_string()
    } else if distinct_authors == 1 {
        "Sole owner. Pair-program a peer through the next change to spread context.".to_string()
    } else if top_share >= 0.70 {
        "Owner concentration ≥70%. Schedule a knowledge-transfer session and add docs.".to_string()
    } else {
        "Some concentration; rotate reviewers across PRs to spread context.".to_string()
    }
}

fn health_envelope(graph_present: bool, blame_present: bool) -> serde_json::Value {
    json!({
        "graph_stale": !graph_present,
        "blame_present": blame_present,
    })
}
