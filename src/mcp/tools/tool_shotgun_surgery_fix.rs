//! `tool_shotgun_surgery_fix` — consolidation recommender for shotgun_surgery smells.
//!
//! For each hub file with `partner_count >= min_partners`, identify the
//! "centroid" of the hub-plus-partners set (highest-PageRank file) and
//! recommend consolidating the scattered logic into it.
//!
//! Soft-fails when git history is disabled for the project — co-change
//! coupling depends on `git_commit_files`, which is opt-in per project
//! via `[git] index_history = true` in `.pgmcp.toml`.

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
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_shotgun_surgery_fix(
    ctx: &SystemContext,
    params: ShotgunSurgeryFixParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .consolidation_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_partners = params.min_partners.unwrap_or(6).max(2);
    let min_coupling = params.min_coupling.unwrap_or(0.2).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(15).max(1);

    debug!(
        tool = "shotgun_surgery_fix",
        project = %params.project,
        min_partners,
        min_coupling,
        limit,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;

    // Pull all coupled-file pairs for the project. The DbClient method gates
    // on `has_commit_files_for_project` and returns Vec::default() when git
    // history isn't indexed.
    let pairs = ctx
        .db()
        .find_coupled_files(&params.project, min_coupling, 2)
        .await
        .map_err(|e| McpError::internal_error(format!("Coupling query failed: {}", e), None))?;

    if pairs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "fixes": [],
                "fix_count": 0,
                "parameters": parameters_echo(&params, min_partners, min_coupling, limit),
                "guidance": "No co-change pairs above threshold. Either git history is disabled \
                             for this project (set `[git] index_history = true` in .pgmcp.toml), \
                             or no shotgun-surgery patterns exist.",
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Build per-file partner lists.
    let mut partners: HashMap<String, HashSet<String>> = HashMap::new();
    for p in &pairs {
        partners
            .entry(p.file_a.clone())
            .or_default()
            .insert(p.file_b.clone());
        partners
            .entry(p.file_b.clone())
            .or_default()
            .insert(p.file_a.clone());
    }

    // Collect hubs (files with >= min_partners co-change partners).
    let mut hubs: Vec<(String, HashSet<String>)> = partners
        .into_iter()
        .filter(|(_, set)| set.len() >= min_partners as usize)
        .collect();
    if hubs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "fixes": [],
                "fix_count": 0,
                "parameters": parameters_echo(&params, min_partners, min_coupling, limit),
                "guidance": format!(
                    "No files with >= {} co-change partners. Project may not exhibit \
                     shotgun-surgery patterns.",
                    min_partners
                ),
                "health": health_envelope(true, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Sort hubs by descending partner count (largest blast-radius first).
    hubs.sort_by_key(|(_, set)| std::cmp::Reverse(set.len()));
    hubs.truncate(limit as usize);

    // Resolve project_id for the file_metrics lookup.
    let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
        .bind(&params.project)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;
    let project_id = project_id.ok_or_else(|| {
        McpError::invalid_params(format!("Project not found: {}", params.project), None)
    })?;

    // Pull pagerank for every file in (hubs ∪ all-partners) — one bulk query.
    let mut all_paths: HashSet<String> = HashSet::new();
    for (hub, partner_set) in &hubs {
        all_paths.insert(hub.clone());
        for p in partner_set {
            all_paths.insert(p.clone());
        }
    }
    let pageranks = fetch_pageranks(pool, project_id, &all_paths).await?;
    let graph_present = !pageranks.is_empty();

    let mut fixes: Vec<serde_json::Value> = Vec::new();
    for (hub, partner_set) in hubs {
        // Compose the candidate set (hub + partners) and pick the centroid.
        let mut candidates: Vec<String> = std::iter::once(hub.clone())
            .chain(partner_set.iter().cloned())
            .collect();
        candidates.sort();
        candidates.dedup();

        let centroid = pick_centroid(&candidates, &pageranks).unwrap_or_else(|| hub.clone());
        let centroid_pagerank = pageranks.get(&centroid).copied().unwrap_or(0.0);

        // The hub itself is one candidate; the others are the "scattered" partners.
        let movers: Vec<String> = candidates
            .iter()
            .filter(|p| **p != centroid)
            .cloned()
            .collect();

        // Severity: more partners = bigger blast radius.
        let severity = if partner_set.len() >= 12 {
            "high"
        } else if partner_set.len() >= 8 {
            "medium"
        } else {
            "low"
        };

        // Build the recommended_fix.
        let mut fix = RecommendedFix::new(FixAction::ConsolidateLogic, params.project.clone())
            .with_confidence(if graph_present { 0.55 } else { 0.40 })
            .with_effort(if movers.len() >= 8 {
                EstimatedEffort::Large
            } else {
                EstimatedEffort::Medium
            })
            .add_location(PathRange {
                path: hub.clone(),
                start_line: 1,
                end_line: 1,
            })
            .add_target(TargetPath {
                path: Some(centroid.clone()),
                ..Default::default()
            })
            .add_step(format!(
                "Consolidate scattered logic into {} (highest PageRank in the cluster: {:.4}).",
                centroid, centroid_pagerank
            ));
        for mover in &movers {
            fix = fix.add_step(format!(
                "Identify the chunks in {} that change together with {}; move them into {}.",
                mover, hub, centroid
            ));
        }
        fix = fix.add_step(format!(
            "Run `find_coupled_files` after consolidation to verify the partner count for {} \
             has dropped below {}.",
            hub, min_partners
        ));
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        fixes.push(json!({
            "hub_file": hub,
            "partner_count": partner_set.len(),
            "centroid_file": centroid,
            "centroid_pagerank": centroid_pagerank,
            "severity": severity,
            "why_it_matters": format!(
                "{} co-changes with {} other files — every change to it ripples widely. \
                 Consolidating into the centroid reduces the change blast-radius.",
                hub,
                partner_set.len()
            ),
            "movers": movers,
            "recommended_fix": fix_json,
        }));
    }

    let total = fixes.len();
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            let pid = crate::mcp::tools::sema_helpers::effects::project_id_opt(
                pool,
                Some(params.project.as_str()),
            )
            .await;
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
        }
        None => serde_json::json!({}),
    };

    let result = json!({
        "effect_breakdown": effect_breakdown,
        "fixes": fixes,
        "fix_count": total,
        "parameters": parameters_echo(&params, min_partners, min_coupling, limit),
        "guidance": "Each hub's `recommended_fix(action=consolidate_logic)` names the centroid \
                     file and lists per-partner movers. Centroid is the file with the highest \
                     PageRank in the cluster.",
        "health": health_envelope(true, graph_present),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "shotgun_surgery_fix",
        fixes = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Helpers
// ============================================================================

/// Bulk-fetch pageranks for a set of relative paths within a project.
async fn fetch_pageranks(
    pool: &sqlx::PgPool,
    project_id: i32,
    paths: &HashSet<String>,
) -> Result<HashMap<String, f64>, McpError> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let path_list: Vec<String> = paths.iter().cloned().collect();
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        pagerank: Option<f64>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fm.pagerank
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND f.relative_path = ANY($2)",
    )
    .bind(project_id)
    .bind(&path_list)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("PageRank lookup failed: {}", e), None))?;
    let map: HashMap<String, f64> = rows
        .into_iter()
        .filter_map(|r| r.pagerank.map(|p| (r.relative_path, p)))
        .collect();
    Ok(map)
}

/// Pick the path with the highest PageRank from a candidate set.
/// Falls back to lexicographic-first when no PageRank data is available.
fn pick_centroid(candidates: &[String], pageranks: &HashMap<String, f64>) -> Option<String> {
    let mut best: Option<(String, f64)> = None;
    for c in candidates {
        let pr = pageranks.get(c).copied().unwrap_or(0.0);
        match &best {
            Some((_, bp)) if pr <= *bp => {}
            _ => best = Some((c.clone(), pr)),
        }
    }
    if best.as_ref().is_some_and(|(_, pr)| *pr > 0.0) {
        best.map(|(p, _)| p)
    } else {
        // No PageRank data: lexicographic-first as a stable tiebreak.
        candidates.iter().min().cloned()
    }
}

fn parameters_echo(
    params: &ShotgunSurgeryFixParams,
    min_partners: i32,
    min_coupling: f64,
    limit: i32,
) -> serde_json::Value {
    json!({
        "project": params.project,
        "min_partners": min_partners,
        "min_coupling": min_coupling,
        "limit": limit,
    })
}

fn health_envelope(git_history_present: bool, graph_present: bool) -> serde_json::Value {
    json!({
        "git_history_present": git_history_present,
        "graph_stale": !graph_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_centroid_prefers_highest_pagerank() {
        let mut prs = HashMap::new();
        prs.insert("a.rs".to_string(), 0.1);
        prs.insert("b.rs".to_string(), 0.5);
        prs.insert("c.rs".to_string(), 0.3);
        let candidates = vec!["a.rs".to_string(), "b.rs".into(), "c.rs".into()];
        assert_eq!(pick_centroid(&candidates, &prs), Some("b.rs".into()));
    }

    #[test]
    fn pick_centroid_falls_back_to_lex_first_when_pagerank_absent() {
        let prs = HashMap::new();
        let candidates = vec!["zeta.rs".to_string(), "alpha.rs".into(), "beta.rs".into()];
        assert_eq!(pick_centroid(&candidates, &prs), Some("alpha.rs".into()));
    }

    #[test]
    fn pick_centroid_handles_empty_input() {
        let prs = HashMap::new();
        assert!(pick_centroid(&[], &prs).is_none());
    }

    #[test]
    fn pick_centroid_uses_partial_pagerank_data() {
        let mut prs = HashMap::new();
        prs.insert("a.rs".to_string(), 0.0); // explicit zero
        prs.insert("c.rs".to_string(), 0.4);
        let candidates = vec!["a.rs".to_string(), "b.rs".into(), "c.rs".into()];
        // c has actual pagerank 0.4; b has none (treated as 0.0); a has 0.0.
        // c wins.
        assert_eq!(pick_centroid(&candidates, &prs), Some("c.rs".into()));
    }
}
