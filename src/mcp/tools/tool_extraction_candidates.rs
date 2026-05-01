//! `tool_extraction_candidates` — strict superset of `refactoring_report`.
//!
//! Builds on the same `find_duplicate_file_pairs` clustering, then layers:
//! - effort: loc_to_extract, call_sites_to_update, files_touched
//! - risk: tier (low/medium/high) + drivers
//! - proposed_api_surface: per-topic function names from c-TF-IDF keywords
//! - recommended_fix(action=extract_module) — agent-executable contract
//!
//! Reads the materialized `cross_project_similarities` table (via
//! `find_duplicate_file_pairs`); requires the 6-hour similarity-scan cron.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
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
use crate::mcp::tools::fix_helpers::{infer_module_name_from_topics, pool_or_err};

pub async fn tool_extraction_candidates(
    ctx: &SystemContext,
    params: ExtractionCandidatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .extraction_candidate_reports
        .fetch_add(1, Ordering::Relaxed);

    let min_sim = params.min_similarity.unwrap_or(0.85).clamp(0.0, 1.0);
    let min_projects = params.min_projects.unwrap_or(2);
    let limit = params.limit.unwrap_or(20).max(1);
    let include_call_sites = params.include_call_sites.unwrap_or(true);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    let worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");
    let main_only = matches!(worktree_filter, "main");
    let risk_threshold = params.risk_threshold.as_deref().unwrap_or("any");

    info!(
        tool = "extraction_candidates",
        min_similarity = min_sim,
        min_projects,
        worktree_filter,
        include_call_sites,
        risk_threshold,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;

    // Pull duplicate file-pairs; this query already enforces same-repo skip
    // and the cross-project (project_id_a != project_id_b) invariant.
    let pairs = ctx
        .db()
        .find_duplicate_file_pairs(
            min_sim,
            params.language.as_deref(),
            limit * 5,
            include_same_repo,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

    if pairs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "candidates": [],
                "total_candidates": 0,
                "parameters": parameters_echo(&params, min_sim, min_projects, limit, worktree_filter, include_same_repo, include_call_sites, risk_threshold),
                "guidance": "No file pairs above threshold. Lower min_similarity or wait for the \
                             6-hour similarity-scan cron to run.",
                "health": health_envelope(false, false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // If main-only, drop pairs whose endpoints aren't in the canonical project set.
    let pairs = if main_only {
        let main_ids: HashSet<i32> = queries::select_main_worktree_projects(pool)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Worktree resolver failed: {}", e), None)
            })?
            .into_iter()
            .collect();
        pairs
            .into_iter()
            .filter(|p| main_ids.contains(&p.project_id_a) && main_ids.contains(&p.project_id_b))
            .collect()
    } else {
        pairs
    };

    // Cluster file-pairs (reuse the existing helper from server.rs).
    let clusters = cluster_file_pairs(&pairs, min_projects);

    // Take the top `limit` clusters before doing call-site / risk lookups.
    let surviving: Vec<&serde_json::Value> = clusters.iter().take(limit as usize).collect();
    if surviving.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "candidates": [],
                "total_candidates": 0,
                "parameters": parameters_echo(&params, min_sim, min_projects, limit, worktree_filter, include_same_repo, include_call_sites, risk_threshold),
                "guidance": format!(
                    "Found pairs but no clusters spanning >= {} projects.",
                    min_projects
                ),
                "health": health_envelope(true, false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Collect all file_ids across surviving clusters for batch metric lookup.
    let mut all_file_ids: Vec<i64> = Vec::new();
    for c in &surviving {
        if let Some(files) = c["files"].as_array() {
            for f in files {
                if let Some(fid) = f["file_id"].as_i64() {
                    all_file_ids.push(fid);
                }
            }
        }
    }
    all_file_ids.sort_unstable();
    all_file_ids.dedup();

    // Call-site counts (resolved + unresolved) — drop when the user opted out.
    let call_sites = if include_call_sites {
        queries::count_call_sites_to_files(pool, &all_file_ids)
            .await
            .map_err(|e| McpError::internal_error(format!("Call-site query failed: {}", e), None))?
    } else {
        Vec::new()
    };
    let mut call_sites_by_file: HashMap<i64, queries::CallSiteCount> = HashMap::new();
    for cs in call_sites {
        call_sites_by_file.insert(cs.file_id, cs);
    }

    // Risk metrics for risk-tier classification.
    let metrics = queries::get_file_risk_metrics(pool, &all_file_ids)
        .await
        .map_err(|e| McpError::internal_error(format!("Risk-metrics query failed: {}", e), None))?;
    let metrics_by_file: HashMap<i64, queries::FileRiskMetrics> =
        metrics.into_iter().map(|m| (m.file_id, m)).collect();
    let graph_present_anywhere = !metrics_by_file.is_empty() || !call_sites_by_file.is_empty();

    // Emit enriched candidate rows.
    let mut candidates: Vec<serde_json::Value> = Vec::new();
    for cluster in surviving {
        let empty = Vec::new();
        let files = cluster["files"].as_array().unwrap_or(&empty);
        let projects: Vec<&str> = cluster["projects"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        // Effort metrics ----------------------------------------------------
        let mut loc_to_extract: i64 = 0;
        let mut call_sites_to_update: i64 = 0;
        let mut unresolved_call_sites: i64 = 0;
        let files_touched = files.len() as i64;
        for f in files {
            if let Some(lines) = f["line_count"].as_i64() {
                loc_to_extract = loc_to_extract.max(lines);
            }
            if let Some(fid) = f["file_id"].as_i64()
                && let Some(cs) = call_sites_by_file.get(&fid)
            {
                call_sites_to_update += cs.importer_count;
                unresolved_call_sites += cs.unresolved_count;
            }
        }

        // Risk classification ----------------------------------------------
        let mut risk_drivers: Vec<String> = Vec::new();
        let mut churn_high_count = 0_i64;
        for f in files {
            if let Some(fid) = f["file_id"].as_i64()
                && let Some(m) = metrics_by_file.get(&fid)
                && m.churn_rate.unwrap_or(0.0) > 2.0
            {
                churn_high_count += 1;
                if let Some(path) = f["relative_path"].as_str() {
                    risk_drivers.push(format!("high churn on {}", path));
                }
            }
        }
        if call_sites_to_update > 50 {
            risk_drivers.push(format!("{} call sites must update", call_sites_to_update));
        }
        if unresolved_call_sites > 0 {
            risk_drivers.push(format!(
                "{} unresolved (Go/Java/C++) imports — counts are approximate",
                unresolved_call_sites
            ));
        }
        let risk_tier = if call_sites_to_update > 50 || churn_high_count > 0 {
            "high"
        } else if call_sites_to_update > 10 || unresolved_call_sites > 0 {
            "medium"
        } else {
            "low"
        };
        if matches!(risk_tier, "high" | "medium" | "low") && !graph_present_anywhere {
            risk_drivers.push("graph metrics absent — risk inference incomplete".to_string());
        }

        // Filter by risk_threshold ------------------------------------------
        if !passes_risk_filter(risk_tier, risk_threshold) {
            continue;
        }

        // Inferred crate name + API surface ---------------------------------
        let paths: Vec<&str> = files
            .iter()
            .filter_map(|f| f["relative_path"].as_str())
            .collect();
        let suggested_crate_name = infer_crate_name(&paths);
        let api_surface = derive_api_surface(&paths);

        let priority = (loc_to_extract as f64)
            * (projects.len() as f64)
            * (call_sites_to_update.max(1) as f64);

        // Build recommended_fix --------------------------------------------
        let project_for_fix = projects.first().copied().unwrap_or("unknown").to_string();
        let mut fix = RecommendedFix::new(FixAction::ExtractModule, project_for_fix)
            .with_confidence(if graph_present_anywhere { 0.65 } else { 0.50 })
            .with_effort(if call_sites_to_update > 50 || files_touched > 6 {
                EstimatedEffort::Large
            } else if call_sites_to_update > 10 || files_touched > 3 {
                EstimatedEffort::Medium
            } else {
                EstimatedEffort::Small
            });
        for f in files {
            if let (Some(path), Some(lc)) = (f["relative_path"].as_str(), f["line_count"].as_i64())
            {
                fix = fix.add_location(PathRange {
                    path: path.to_string(),
                    start_line: 1,
                    end_line: lc.max(1) as u32,
                });
            }
        }
        let proposed_path = format!("shared/{}/lib.rs", suggested_crate_name);
        fix = fix
            .add_target(TargetPath {
                suggested_new_path: Some(proposed_path),
                ..Default::default()
            })
            .add_step(format!(
                "Extract these {} files into a new shared crate `{}`. \
                 Estimated work: {} LOC, {} call sites to update, {} files touched.",
                files.len(),
                suggested_crate_name,
                loc_to_extract,
                call_sites_to_update,
                files_touched
            ));
        if unresolved_call_sites > 0 {
            fix = fix.add_step(format!(
                "Note: {} import sites are in Go/Java/C/C++ projects whose imports aren't yet \
                 resolved by pgmcp. Verify those manually before deletion.",
                unresolved_call_sites
            ));
        }
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        candidates.push(json!({
            "suggested_crate_name": suggested_crate_name,
            "language": cluster["language"],
            "projects": projects,
            "project_count": projects.len(),
            "files": files,
            "avg_similarity": cluster["avg_similarity"],
            "estimated_shared_lines": loc_to_extract,
            "effort": {
                "loc_to_extract": loc_to_extract,
                "call_sites_to_update": call_sites_to_update,
                "unresolved_call_sites": unresolved_call_sites,
                "files_touched": files_touched,
            },
            "risk": {
                "tier": risk_tier,
                "drivers": risk_drivers,
            },
            "proposed_api_surface": api_surface,
            "recommended_fix": fix_json,
            "priority_score": format!("{:.2}", priority),
        }));
    }

    // Sort by priority descending, then loc_to_extract.
    candidates.sort_by(|a, b| {
        let pa: f64 = a["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let pb: f64 = b["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        pb.partial_cmp(&pa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b["effort"]["loc_to_extract"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["effort"]["loc_to_extract"].as_i64().unwrap_or(0))
            })
    });

    let total = candidates.len();
    let result = json!({
        "candidates": candidates,
        "total_candidates": total,
        "parameters": parameters_echo(&params, min_sim, min_projects, limit, worktree_filter, include_same_repo, include_call_sites, risk_threshold),
        "guidance": format!(
            "Top {} extraction candidates ranked by loc_to_extract × project_count × call_sites. \
             Each carries a typed `recommended_fix(action=extract_module)`. Risk tier inferred \
             from churn ({}+ files high-churn) and call-site count.",
            total,
            "any"
        ),
        "health": health_envelope(true, graph_present_anywhere, false),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "extraction_candidates",
        candidates = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Helpers
// ============================================================================

fn passes_risk_filter(tier: &str, threshold: &str) -> bool {
    let order = |t: &str| match t {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        _ => 0,
    };
    match threshold {
        "any" => true,
        "low" => order(tier) <= 0,
        "low-med" => order(tier) <= 1,
        _ => true,
    }
}

/// Derive a proposed API surface from the file paths in a cluster. For the
/// regex-only stage (Phase 2), we use the file basenames as suggested
/// function names. Once Tier 0e (tree-sitter) lands, we'll switch to actual
/// `pub` symbol extraction from `file_symbols`.
fn derive_api_surface(paths: &[&str]) -> Vec<serde_json::Value> {
    let mut surface: Vec<serde_json::Value> = Vec::new();
    for p in paths {
        let basename = p.rsplit('/').next().unwrap_or(p);
        let stem = basename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(basename);
        let snake = stem
            .chars()
            .flat_map(|c| {
                if c.is_ascii_uppercase() {
                    let mut v = vec!['_'];
                    for low in c.to_lowercase() {
                        v.push(low);
                    }
                    v
                } else if c == '-' {
                    vec!['_']
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
            .trim_start_matches('_')
            .to_string();
        if snake.is_empty() {
            continue;
        }
        surface.push(json!({
            "name": snake,
            "from_file": p,
        }));
    }
    surface
}

#[allow(clippy::too_many_arguments)] // Echoing the params verbatim is the simplest contract.
fn parameters_echo(
    params: &ExtractionCandidatesParams,
    min_sim: f64,
    min_projects: usize,
    limit: i32,
    worktree_filter: &str,
    include_same_repo: bool,
    include_call_sites: bool,
    risk_threshold: &str,
) -> serde_json::Value {
    json!({
        "min_similarity": min_sim,
        "min_projects": min_projects,
        "language": params.language,
        "limit": limit,
        "worktree_filter": worktree_filter,
        "include_same_repo": include_same_repo,
        "include_call_sites": include_call_sites,
        "risk_threshold": risk_threshold,
    })
}

fn health_envelope(
    similarity_present: bool,
    graph_present: bool,
    symbols_present: bool,
) -> serde_json::Value {
    json!({
        "similarity_stale": !similarity_present,
        "graph_stale": !graph_present,
        "symbols_present": symbols_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_risk_filter_dispatches() {
        assert!(passes_risk_filter("low", "any"));
        assert!(passes_risk_filter("high", "any"));
        assert!(passes_risk_filter("low", "low"));
        assert!(!passes_risk_filter("medium", "low"));
        assert!(passes_risk_filter("low", "low-med"));
        assert!(passes_risk_filter("medium", "low-med"));
        assert!(!passes_risk_filter("high", "low-med"));
    }

    #[test]
    fn derive_api_surface_converts_kebab_and_camel_to_snake() {
        let paths = vec!["src/foo-bar.rs", "src/BazQux.py"];
        let surface = derive_api_surface(&paths);
        assert_eq!(surface[0]["name"], "foo_bar");
        assert_eq!(surface[1]["name"], "baz_qux");
    }

    #[test]
    fn derive_api_surface_strips_extension_and_dir() {
        let paths = vec!["a/b/c.rs"];
        let surface = derive_api_surface(&paths);
        assert_eq!(surface[0]["name"], "c");
        assert_eq!(surface[0]["from_file"], "a/b/c.rs");
    }

    #[test]
    fn derive_api_surface_skips_files_with_empty_stem() {
        let paths = vec!["dir/.hidden"];
        let surface = derive_api_surface(&paths);
        assert!(surface.is_empty(), "rsplit_once on \".hidden\" → stem=\"\"");
    }
}
