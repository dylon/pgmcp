//! `tool_stale_zombie` — graph + history-based dead-code identification.
//!
//! Finds files matching ALL of:
//! - bottom 25% PageRank (configurable)
//! - in_degree <= 1 (only one or zero importers)
//! - days_since_last_change > 540 (≈18 months) by default
//!
//! Distinct from `find_orphans` (which is topic-membership based). This
//! combines structural centrality (PageRank, in_degree) with authorial
//! abandonment (long-idle + low author count).
//!
//! Soft-fails when `file_metrics` is empty: returns an empty list with
//! `health.graph_stale: true` and guidance to wait for the graph-analysis cron.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_stale_zombie(
    ctx: &SystemContext,
    params: StaleZombieParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().zombie_scans.fetch_add(1, Ordering::Relaxed);

    let min_days_idle = params.min_days_idle.unwrap_or(540).max(0);
    let max_pagerank_pct = params.max_pagerank_pct.unwrap_or(0.25).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(30).max(1);

    debug!(
        tool = "stale_zombie_detector",
        project = %params.project,
        min_days_idle,
        max_pagerank_pct,
        limit,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let candidates = queries::find_zombie_candidates(
        pool,
        &params.project,
        min_days_idle,
        max_pagerank_pct,
        limit,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Zombie query failed: {}", e), None))?;

    // If file_metrics is empty for the project, every row will have
    // pagerank=NULL and PERCENT_RANK=0; the in_degree=0 filter still admits
    // many files. Guard against that explicitly: when none of the candidates
    // have a non-null pagerank, treat as graph-stale.
    let graph_present = candidates.iter().any(|c| c.pagerank.is_some());

    if candidates.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "candidates": [],
                "total_candidates": 0,
                "parameters": parameters_echo(&params, min_days_idle, max_pagerank_pct, limit),
                "guidance": "No zombie candidates. Either the project is healthy or filters are \
                             too aggressive — try lowering min_days_idle.",
                "health": health_envelope(graph_present, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    let mut output: Vec<serde_json::Value> = Vec::new();
    for cand in &candidates {
        // Severity: in_degree=0 + bottom 5% pagerank → high (very likely dead)
        // in_degree=0 + idle > 2 years → medium-high
        // in_degree=1 → medium (move the symbol then delete)
        let in_deg = cand.in_degree.unwrap_or(0);
        let days_idle = cand.days_since_last_change.unwrap_or(0);
        let severity = if in_deg == 0 && (cand.pagerank_pct < 0.05 || days_idle > 730) {
            "high"
        } else if in_deg == 0 {
            "medium"
        } else {
            "low"
        };

        let action = if in_deg == 0 {
            FixAction::DeleteFile
        } else {
            FixAction::MoveFunction
        };
        // Confidence drops when graph data is partial.
        let confidence = if graph_present { 0.75 } else { 0.40 };
        let mut fix = RecommendedFix::new(action, params.project.clone())
            .with_confidence(confidence)
            .with_effort(EstimatedEffort::Small)
            .add_location(PathRange {
                path: cand.relative_path.clone(),
                start_line: 1,
                end_line: cand.line_count.max(1) as u32,
            });

        match action {
            FixAction::DeleteFile => {
                fix = fix
                    .add_target(TargetPath {
                        path: Some(cand.relative_path.clone()),
                        ..Default::default()
                    })
                    .add_step(format!(
                        "Verify with `grep -r '{}'` outside the index.",
                        path_to_module_token(&cand.relative_path)
                    ))
                    .add_step(format!(
                        "Remove the `pub mod` declaration that registers {}.",
                        cand.relative_path
                    ))
                    .add_step(format!("Delete the file at {}.", cand.relative_path));
            }
            FixAction::MoveFunction => {
                fix = fix.add_step(format!(
                    "Single importer references {}. Inline the symbol(s) into that importer, \
                     then delete the file.",
                    cand.relative_path
                ));
            }
            _ => {}
        }
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        output.push(json!({
            "path": cand.relative_path,
            "in_degree": in_deg,
            "days_idle": days_idle,
            "authors": cand.author_count,
            "commits": cand.commit_count,
            "pagerank": cand.pagerank,
            "pagerank_percentile": format!("{:.4}", cand.pagerank_pct),
            "severity": severity,
            "why_it_matters": format!(
                "Bottom {}% PageRank, {} importer(s), untouched {} days. Likely candidates for \
                 removal — surface so the team can decide.",
                (cand.pagerank_pct * 100.0).round() as i64,
                in_deg,
                days_idle
            ),
            "recommended_fix": fix_json,
        }));
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
        "candidates": output,
        "total_candidates": output.len(),
        "parameters": parameters_echo(&params, min_days_idle, max_pagerank_pct, limit),
        "guidance": "Each candidate has a typed `recommended_fix`. \
                     in_degree=0 → delete_file (verify first); in_degree=1 → move_function then delete.",
        "health": health_envelope(graph_present, true),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "stale_zombie_detector",
        candidates = output.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

/// Crude module-token from a path — used to compose grep verification steps.
/// E.g. `src/legacy/foo.rs` → `legacy::foo` (Rust); `src/old.py` → `old`.
/// Best-effort; the agent reading the step is expected to adjust per-language.
fn path_to_module_token(path: &str) -> String {
    let stem = path.rsplit('/').next().unwrap_or(path);
    let stem = stem.rsplit_once('.').map(|(s, _)| s).unwrap_or(stem);
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if dir.is_empty() {
        stem.to_string()
    } else {
        let leaf_dir = dir.rsplit('/').next().unwrap_or(dir);
        format!("{}::{}", leaf_dir, stem)
    }
}

fn parameters_echo(
    params: &StaleZombieParams,
    min_days_idle: i32,
    max_pagerank_pct: f64,
    limit: i32,
) -> serde_json::Value {
    json!({
        "project": params.project,
        "min_days_idle": min_days_idle,
        "max_pagerank_pct": max_pagerank_pct,
        "limit": limit,
    })
}

fn health_envelope(graph_present: bool, candidates_returned: bool) -> serde_json::Value {
    json!({
        "graph_stale": !graph_present,
        "candidates_present": candidates_returned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_module_token_combines_dir_and_stem() {
        assert_eq!(path_to_module_token("src/legacy/foo.rs"), "legacy::foo");
        assert_eq!(path_to_module_token("foo.py"), "foo");
        assert_eq!(path_to_module_token("a/b/c/d.ts"), "c::d");
    }

    #[test]
    fn path_to_module_token_handles_no_extension() {
        assert_eq!(path_to_module_token("Makefile"), "Makefile");
        assert_eq!(path_to_module_token("scripts/run"), "scripts::run");
    }
}
