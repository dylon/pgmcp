//! `tool_fix_circular_dependency` — propose a specific edge to break per cycle.
//!
//! For each Tarjan SCC, enumerates simple cycles (capped by max_cycle_length)
//! and selects a "fix edge" via the SDP heuristic: the edge from the
//! more-unstable endpoint to the more-stable endpoint is the natural
//! inversion target. When both ends are similar, we recommend
//! `extract_interface` on the less-coupled side instead.
//!
//! The full per-edge PageRank-delta approach is deferred to a Tier-0e
//! follow-up (it's O(E×iters×N) — feasible only with the symbol-reference
//! data filtering down candidate edges).

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use petgraph::Direction;
use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::graph::CodeGraph;
use crate::graph::algorithms::{extract_simple_cycles, find_cycles};
use crate::graph::metrics::{ModuleMetrics, compute_module_metrics, update_abstractness};
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::{load_import_graph, lookup_project_id};

pub async fn tool_fix_circular_dependency(
    ctx: &SystemContext,
    params: FixCircularDependencyParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().cycle_fix_scans.fetch_add(1, Ordering::Relaxed);

    let max_cycle_length = params.max_cycle_length.unwrap_or(10).max(2) as usize;
    let limit = params.limit.unwrap_or(20).max(1) as usize;
    let prefer_strategy = params.prefer_strategy.as_deref().unwrap_or("auto");

    debug!(
        tool = "fix_circular_dependency",
        project = %params.project,
        max_cycle_length,
        limit,
        prefer_strategy,
        "MCP tool invoked",
    );

    let project_id = lookup_project_id(ctx, &params.project)
        .await?
        .ok_or_else(|| {
            McpError::invalid_params(format!("Project not found: {}", params.project), None)
        })?;
    let bundle = load_import_graph(ctx, project_id).await?;

    let sccs = find_cycles(&bundle.graph.graph);
    if sccs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "fixes": [],
                "scc_count": 0,
                "parameters": parameters_echo(&params, max_cycle_length, limit, prefer_strategy),
                "guidance": "No cycles detected — project's import graph is acyclic.",
                "health": health_envelope(true, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Compute module metrics once; needed for SDP-based strategy selection.
    let module_metrics = compute_module_metrics(&bundle.graph, 2);
    let mut file_abstractions: HashMap<String, bool> = HashMap::new();
    for fm in &bundle.file_metas {
        let is_abs = fm.relative_path.contains("trait")
            || fm.relative_path.contains("interface")
            || fm.relative_path.contains("abstract")
            || fm.relative_path.ends_with("mod.rs");
        file_abstractions.insert(fm.relative_path.clone(), is_abs);
    }
    let mut mm = module_metrics;
    update_abstractness(&mut mm, &file_abstractions);
    let metric_by_module: HashMap<&str, &ModuleMetrics> =
        mm.iter().map(|m| (m.module_path.as_str(), m)).collect();

    let mut fixes: Vec<serde_json::Value> = Vec::new();
    for scc in &sccs {
        let simple = extract_simple_cycles(&bundle.graph.graph, scc, max_cycle_length);
        for cycle in simple.iter().take(limit) {
            if cycle.len() < 2 {
                continue;
            }

            // For each consecutive (u, v) in the cycle, score it as a fix-edge candidate.
            // Higher score = better candidate for breaking.
            let edges: Vec<(NodeIndex, NodeIndex)> = (0..cycle.len())
                .map(|i| (cycle[i], cycle[(i + 1) % cycle.len()]))
                .collect();

            let scored: Vec<EdgeScore> = edges
                .iter()
                .map(|&(u, v)| score_edge(&bundle.graph, u, v, &metric_by_module, prefer_strategy))
                .collect();
            let best_idx = scored
                .iter()
                .enumerate()
                .max_by(|a, b| {
                    a.1.score
                        .partial_cmp(&b.1.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            let best = &scored[best_idx];

            let cycle_paths: Vec<String> = cycle
                .iter()
                .filter_map(|n| {
                    bundle
                        .graph
                        .graph
                        .node_weight(*n)
                        .map(|fn_node| fn_node.relative_path.clone())
                })
                .collect();

            let source_path = bundle
                .graph
                .graph
                .node_weight(best.source)
                .map(|f| f.relative_path.clone())
                .unwrap_or_default();
            let target_path = bundle
                .graph
                .graph
                .node_weight(best.target)
                .map(|f| f.relative_path.clone())
                .unwrap_or_default();

            // Build the recommended_fix.
            let action = best.action;
            let mut fix = RecommendedFix::new(action, params.project.clone())
                .with_confidence(if best.has_metrics { 0.65 } else { 0.45 })
                .with_effort(if cycle.len() >= 5 {
                    EstimatedEffort::Large
                } else {
                    EstimatedEffort::Medium
                });
            for path in &cycle_paths {
                fix = fix.add_location(PathRange {
                    path: path.clone(),
                    start_line: 1,
                    end_line: 1,
                });
            }
            match action {
                FixAction::InvertDependency => {
                    fix = fix
                        .add_step(format!(
                            "Invert the import: {} currently depends on {}; the more-stable side \
                             should expose a trait/interface that the less-stable side depends on.",
                            source_path, target_path,
                        ))
                        .add_step(format!(
                            "Define the trait in {}; move the existing implementation to a new \
                             impl block.",
                            target_path,
                        ))
                        .add_step(format!(
                            "Replace {}'s direct import of {} with the trait import.",
                            source_path, target_path,
                        ));
                }
                FixAction::ExtractInterface => {
                    let proposed_path = derive_port_filename(&target_path);
                    fix = fix
                        .add_target(TargetPath {
                            suggested_new_path: Some(proposed_path.clone()),
                            ..Default::default()
                        })
                        .add_step(format!(
                            "Extract a trait/interface from {}'s public surface into {}.",
                            target_path, proposed_path,
                        ))
                        .add_step(format!(
                            "Update {}'s import: depend on the trait, not the concrete type.",
                            source_path,
                        ))
                        .add_step(
                            "Verify the cycle is broken via `circular_dependencies` and \
                             `architecture_violations`."
                                .to_string(),
                        );
                }
                _ => {}
            }
            let fix_json = serde_json::to_value(&fix).map_err(|e| {
                McpError::internal_error(format!("Fix serialization failed: {}", e), None)
            })?;

            fixes.push(json!({
                "cycle_files": cycle_paths,
                "cycle_length": cycle.len(),
                "edge_to_break": {
                    "source": source_path,
                    "target": target_path,
                },
                "severity": "critical",
                "rationale": best.rationale.clone(),
                "why_it_matters": "Cycles inflate build time, prevent isolated testing, and \
                                   cascade refactor blast-radius.",
                "recommended_fix": fix_json,
            }));
            if fixes.len() >= limit {
                break;
            }
        }
        if fixes.len() >= limit {
            break;
        }
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
        "scc_count": sccs.len(),
        "fix_count": total,
        "fixes": fixes,
        "parameters": parameters_echo(&params, max_cycle_length, limit, prefer_strategy),
        "guidance": format!(
            "{} SCCs found. For each cycle, the edge with the highest break-score is selected; \
             the recommended_fix dispatches on action (invert_dependency vs extract_interface). \
             Confidence drops to 0.45 when module metrics are unavailable.",
            sccs.len()
        ),
        "health": health_envelope(true, !mm.is_empty()),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "fix_circular_dependency",
        scc_count = sccs.len(),
        fixes = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Edge scoring
// ============================================================================

#[derive(Debug, Clone)]
struct EdgeScore {
    source: NodeIndex,
    target: NodeIndex,
    score: f64,
    action: FixAction,
    has_metrics: bool,
    rationale: String,
}

/// Score a candidate fix-edge `(u, v)` in a cycle. Higher score = better
/// candidate for breaking. Strategy heuristic:
///
/// - When both endpoints have module metrics: pick the action by SDP
///   (invert when target is more abstract / stable).
/// - When metrics are absent: fall back to the lower-fanin edge (less
///   disruption) and recommend `extract_interface`.
fn score_edge(
    code_graph: &CodeGraph,
    u: NodeIndex,
    v: NodeIndex,
    metric_by_module: &HashMap<&str, &ModuleMetrics>,
    prefer_strategy: &str,
) -> EdgeScore {
    let u_path = code_graph
        .graph
        .node_weight(u)
        .map(|f| f.relative_path.as_str())
        .unwrap_or("");
    let v_path = code_graph
        .graph
        .node_weight(v)
        .map(|f| f.relative_path.as_str())
        .unwrap_or("");
    let u_module = u_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let v_module = v_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let u_metric = metric_by_module.get(u_module).copied();
    let v_metric = metric_by_module.get(v_module).copied();
    let has_metrics = u_metric.is_some() && v_metric.is_some();

    // Base score: lower in-degree on the edge's source side = less
    // disruption to break (fewer downstream importers cascade).
    let u_in_degree = code_graph
        .graph
        .neighbors_directed(u, Direction::Incoming)
        .count() as f64;
    let v_in_degree = code_graph
        .graph
        .neighbors_directed(v, Direction::Incoming)
        .count() as f64;
    // Prefer edges whose target is the more-imported (stable) one;
    // the source is the natural "consumer" that should be inverted.
    let in_degree_advantage = (v_in_degree - u_in_degree).max(0.0);
    let base_score = 1.0 + in_degree_advantage;

    // Decide action. SDP: invert if target is more abstract / less unstable.
    let (action, sdp_bonus, rationale): (FixAction, f64, String) = match (u_metric, v_metric) {
        (Some(um), Some(vm)) => {
            let v_more_abstract = vm.abstractness > um.abstractness;
            let v_more_stable = vm.instability < um.instability;
            if matches!(prefer_strategy, "inversion")
                || (matches!(prefer_strategy, "auto") && v_more_abstract && v_more_stable)
            {
                (
                    FixAction::InvertDependency,
                    1.5,
                    format!(
                        "Target {} (A={:.2}, I={:.2}) is more abstract and more stable than \
                         source {} (A={:.2}, I={:.2}); flipping the dependency moves the \
                         abstraction barrier where it belongs.",
                        v_module,
                        vm.abstractness,
                        vm.instability,
                        u_module,
                        um.abstractness,
                        um.instability
                    ),
                )
            } else {
                (
                    FixAction::ExtractInterface,
                    1.0,
                    format!(
                        "Extract a trait/interface from {}'s surface so {} depends on the \
                         abstraction, not the concrete type. (Target A={:.2} I={:.2}; \
                         source A={:.2} I={:.2}.)",
                        v_module,
                        u_module,
                        vm.abstractness,
                        vm.instability,
                        um.abstractness,
                        um.instability
                    ),
                )
            }
        }
        _ => (
            FixAction::ExtractInterface,
            0.0,
            format!(
                "Module metrics not available; recommend extracting a trait/interface from {}'s \
                 public surface so {} depends on the abstraction.",
                v_module, u_module
            ),
        ),
    };

    EdgeScore {
        source: u,
        target: v,
        score: base_score + sdp_bonus,
        action,
        has_metrics,
        rationale,
    }
}

/// Derive a `_port` filename for an extract_interface target.
/// `src/foo/bar.rs` → `src/foo/bar_port.rs`.
fn derive_port_filename(path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = match basename.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (basename, ""),
    };
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let new_basename = if ext.is_empty() {
        format!("{}_port", stem)
    } else {
        format!("{}_port.{}", stem, ext)
    };
    if dir.is_empty() {
        new_basename
    } else {
        format!("{}/{}", dir, new_basename)
    }
}

fn parameters_echo(
    params: &FixCircularDependencyParams,
    max_cycle_length: usize,
    limit: usize,
    prefer_strategy: &str,
) -> serde_json::Value {
    json!({
        "project": params.project,
        "max_cycle_length": max_cycle_length,
        "limit": limit,
        "prefer_strategy": prefer_strategy,
    })
}

fn health_envelope(graph_present: bool, metrics_present: bool) -> serde_json::Value {
    json!({
        "graph_stale": !graph_present,
        "metrics_present": metrics_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_port_filename_appends_port_before_extension() {
        assert_eq!(
            derive_port_filename("src/foo/bar.rs"),
            "src/foo/bar_port.rs"
        );
        assert_eq!(derive_port_filename("a.py"), "a_port.py");
    }

    #[test]
    fn derive_port_filename_no_extension() {
        assert_eq!(derive_port_filename("Makefile"), "Makefile_port");
        assert_eq!(derive_port_filename("scripts/run"), "scripts/run_port");
    }
}
