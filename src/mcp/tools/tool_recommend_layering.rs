//! `tool_recommend_layering` — propose a layered architecture; flag every cross-layer import.
//!
//! Algorithm (regex-only, pre-Tier-0e):
//! 1. Run Louvain on the import graph to get communities.
//! 2. Compute median instability per community.
//! 3. Sort communities by median instability ascending; equal-mass quantile-bin
//!    into N layers (default 4).
//! 4. Walk every edge; flag downward-skip-N (>1 layer down) and upward edges.
//! 5. Per violation, dispatch action: `add_anti_corruption_layer` for skip-N,
//!    `invert_dependency` for upward, `move_function` for short-leaf moves.
//!
//! Layer naming: when `layer_names` is supplied (length = num_layers, top→bottom),
//! use those. Otherwise, label by instability percentile heuristically.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::graph::CodeGraph;
use crate::graph::algorithms::louvain_communities;
use crate::graph::metrics::{ModuleMetrics, compute_module_metrics};
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::{load_import_graph, lookup_project_id};

const DEFAULT_LAYER_NAMES_4: &[&str] = &["api", "service", "domain", "data"];

pub async fn tool_recommend_layering(
    ctx: &SystemContext,
    params: RecommendLayeringParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().layering_scans.fetch_add(1, Ordering::Relaxed);

    let num_layers = params.num_layers.unwrap_or(4).max(2);
    let severity_threshold = params.severity_threshold.as_deref().unwrap_or("medium");
    let limit = params.limit.unwrap_or(50).max(1) as usize;

    debug!(
        tool = "recommend_layering",
        project = %params.project,
        num_layers,
        severity_threshold,
        limit,
        "MCP tool invoked",
    );

    let project_id = lookup_project_id(ctx, &params.project)
        .await?
        .ok_or_else(|| {
            McpError::invalid_params(format!("Project not found: {}", params.project), None)
        })?;
    let bundle = load_import_graph(ctx, project_id).await?;

    if bundle.graph.node_count() == 0 {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "proposed_layers": [],
                "violation_count": 0,
                "violations": [],
                "parameters": parameters_echo(&params, num_layers, severity_threshold, limit),
                "guidance": "Empty import graph — nothing to layer.",
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    let louvain = louvain_communities(&bundle.graph, 1.0);
    if louvain.num_communities < num_layers {
        // Not enough communities to bucket distinctly — collapse to actual count
        // and flag as a low-confidence run.
        debug!(
            communities = louvain.num_communities,
            requested_layers = num_layers,
            "fewer communities than requested layers; collapsing"
        );
    }
    let effective_layers = num_layers.min(louvain.num_communities.max(1));

    // Module metrics for instability lookup (used to bucket communities).
    let module_metrics = compute_module_metrics(&bundle.graph, 2);
    let module_instability: HashMap<&str, f64> = module_metrics
        .iter()
        .map(|m| (m.module_path.as_str(), m.instability))
        .collect();

    // Compute median instability per community.
    let mut by_community: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for (node, &comm) in &louvain.communities {
        by_community.entry(comm).or_default().push(*node);
    }
    let mut community_score: Vec<(usize, f64, Vec<NodeIndex>)> = by_community
        .into_iter()
        .map(|(comm, nodes)| {
            let mut instabilities: Vec<f64> = nodes
                .iter()
                .filter_map(|n| {
                    bundle
                        .graph
                        .graph
                        .node_weight(*n)
                        .map(|f| f.module.as_str())
                        .and_then(|m| module_instability.get(m).copied())
                })
                .collect();
            instabilities.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = if instabilities.is_empty() {
                0.5
            } else {
                instabilities[instabilities.len() / 2]
            };
            (comm, median, nodes)
        })
        .collect();
    // Sort communities by ascending median instability (most stable first → bottom layer).
    community_score.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    // Bucket communities into layers (equal-count bins). Layer 0 = most stable
    // (data); layer N-1 = most unstable (UI).
    let n_comms = community_score.len().max(1);
    let mut community_to_layer: HashMap<usize, usize> = HashMap::new();
    for (rank, (comm, _, _)) in community_score.iter().enumerate() {
        let layer = (rank * effective_layers) / n_comms;
        community_to_layer.insert(*comm, layer.min(effective_layers.saturating_sub(1)));
    }

    // Resolve layer names. User override > heuristic.
    let layer_names: Vec<String> = params
        .layer_names
        .as_ref()
        .filter(|v| v.len() == effective_layers)
        .cloned()
        .unwrap_or_else(|| {
            // Heuristic: layer 0 is "data" (most stable), top is "ui". For 4 layers, use
            // the canonical web stack; for other Ns, just number them.
            if effective_layers == 4 {
                DEFAULT_LAYER_NAMES_4
                    .iter()
                    .rev()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            } else {
                (0..effective_layers)
                    .map(|i| format!("layer_{}", i))
                    .collect()
            }
        });

    // Build per-layer summary.
    let mut proposed_layers: Vec<serde_json::Value> = Vec::new();
    for (layer, layer_name) in layer_names.iter().enumerate().take(effective_layers) {
        let comms_in_layer: Vec<usize> = community_to_layer
            .iter()
            .filter(|&(_, &l)| l == layer)
            .map(|(&c, _)| c)
            .collect();
        let median_instability = community_score
            .iter()
            .filter(|(c, _, _)| comms_in_layer.contains(c))
            .map(|(_, m, _)| *m)
            .sum::<f64>()
            / comms_in_layer.len().max(1) as f64;
        let module_paths: Vec<String> = community_score
            .iter()
            .filter(|(c, _, _)| comms_in_layer.contains(c))
            .flat_map(|(_, _, nodes)| {
                nodes
                    .iter()
                    .filter_map(|n| bundle.graph.graph.node_weight(*n).map(|f| f.module.clone()))
                    .collect::<HashSet<_>>()
            })
            .collect();
        let mut sorted_paths: Vec<String> = module_paths.into_iter().collect();
        sorted_paths.sort();
        sorted_paths.dedup();

        proposed_layers.push(json!({
            "layer_index": layer,
            "name": layer_name,
            "communities": comms_in_layer,
            "modules": sorted_paths,
            "median_instability": format!("{:.4}", median_instability),
        }));
    }

    // Walk every edge; flag violations.
    let mut violations: Vec<serde_json::Value> = Vec::new();
    let severity_order = |s: &str| -> i32 {
        ["low", "medium", "high", "critical"]
            .iter()
            .position(|&x| x == s)
            .unwrap_or(0) as i32
    };
    let threshold = severity_order(severity_threshold);

    for edge in bundle.graph.graph.edge_references() {
        let u = edge.source();
        let v = edge.target();
        let u_comm = match louvain.communities.get(&u) {
            Some(&c) => c,
            None => continue,
        };
        let v_comm = match louvain.communities.get(&v) {
            Some(&c) => c,
            None => continue,
        };
        let u_layer = community_to_layer.get(&u_comm).copied().unwrap_or(0) as i32;
        let v_layer = community_to_layer.get(&v_comm).copied().unwrap_or(0) as i32;
        // Convention: higher layer number = upper layer (UI). Downward = source layer > target layer.
        // Allowed: source layer == target layer + 1 (one step down). One-step-up calls are fine
        // *if* the abstraction is a trait — we can't tell here, so we flag every upward edge.
        let layers_crossed = u_layer - v_layer;
        let (severity, action_kind, description) = if layers_crossed > 1 {
            // Skip-N downward.
            let sev = match layers_crossed {
                2 => "medium",
                3 => "high",
                _ => "critical",
            };
            (
                sev,
                FixAction::AddAntiCorruptionLayer,
                "downward skip-layer",
            )
        } else if layers_crossed < 0 {
            ("high", FixAction::InvertDependency, "upward dependency")
        } else {
            continue; // healthy: same layer or one step down
        };
        if severity_order(severity) < threshold {
            continue;
        }
        let source_path = bundle
            .graph
            .graph
            .node_weight(u)
            .map(|f| f.relative_path.clone())
            .unwrap_or_default();
        let target_path = bundle
            .graph
            .graph
            .node_weight(v)
            .map(|f| f.relative_path.clone())
            .unwrap_or_default();

        let source_layer_name = layer_names
            .get(u_layer.max(0) as usize)
            .cloned()
            .unwrap_or_default();
        let target_layer_name = layer_names
            .get(v_layer.max(0) as usize)
            .cloned()
            .unwrap_or_default();

        let mut fix = RecommendedFix::new(action_kind, params.project.clone())
            .with_confidence(0.50)
            .with_effort(EstimatedEffort::Medium)
            .add_location(PathRange {
                path: source_path.clone(),
                start_line: 1,
                end_line: 1,
            });

        match action_kind {
            FixAction::AddAntiCorruptionLayer => {
                let acl_path = derive_acl_filename(&target_path, &source_layer_name);
                fix = fix
                    .add_target(TargetPath {
                        suggested_new_path: Some(acl_path.clone()),
                        ..Default::default()
                    })
                    .add_step(format!(
                        "Insert an anti-corruption layer at {} so {} doesn't reach into {} \
                         directly. The ACL exposes a layer-appropriate API; {} talks to it; \
                         it forwards to {}.",
                        acl_path, source_path, target_path, source_path, target_path
                    ));
            }
            FixAction::InvertDependency => {
                fix = fix.add_step(format!(
                    "Upward dependency: {} (layer '{}') imports from {} (layer '{}'). \
                     Invert by introducing a trait/interface in the lower layer that the upper \
                     layer implements (or owns); the import direction now goes downward.",
                    source_path, source_layer_name, target_path, target_layer_name,
                ));
            }
            _ => {}
        }
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        violations.push(json!({
            "source": source_path,
            "target": target_path,
            "source_layer": source_layer_name,
            "target_layer": target_layer_name,
            "layers_crossed": layers_crossed.abs(),
            "severity": severity,
            "kind": description,
            "why_it_matters": format!(
                "{}: blurs layer boundaries and inflates change blast-radius.",
                description
            ),
            "recommended_fix": fix_json,
        }));
        if violations.len() >= limit {
            break;
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
        "proposed_layers": proposed_layers,
        "violation_count": violations.len(),
        "violations": violations,
        "parameters": parameters_echo(&params, num_layers, severity_threshold, limit),
        "guidance": format!(
            "Inferred {} layers via Louvain + median-instability quantile binning. \
             {} cross-layer violations at severity >= '{}'. Layer-naming is heuristic; pass \
             `layer_names` to override.",
            effective_layers,
            violations.len(),
            severity_threshold
        ),
        "health": health_envelope(true, !module_metrics.is_empty()),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "recommend_layering",
        layers = effective_layers,
        violations = violations.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Helpers
// ============================================================================

fn derive_acl_filename(target_path: &str, source_layer_name: &str) -> String {
    let basename = target_path.rsplit('/').next().unwrap_or(target_path);
    let (stem, ext) = match basename.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (basename, ""),
    };
    let dir = target_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let new_basename = if ext.is_empty() {
        format!("{}_{}_acl", source_layer_name, stem)
    } else {
        format!("{}_{}_acl.{}", source_layer_name, stem, ext)
    };
    if dir.is_empty() {
        new_basename
    } else {
        format!("{}/{}", dir, new_basename)
    }
}

fn parameters_echo(
    params: &RecommendLayeringParams,
    num_layers: usize,
    severity_threshold: &str,
    limit: usize,
) -> serde_json::Value {
    json!({
        "project": params.project,
        "num_layers": num_layers,
        "severity_threshold": severity_threshold,
        "limit": limit,
        "layer_names": params.layer_names,
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
    fn derive_acl_filename_inserts_layer_and_acl() {
        assert_eq!(
            derive_acl_filename("src/data/pg.rs", "ui"),
            "src/data/ui_pg_acl.rs"
        );
        assert_eq!(derive_acl_filename("Makefile", "ui"), "ui_Makefile_acl");
    }

    #[test]
    fn derive_acl_filename_no_directory() {
        assert_eq!(derive_acl_filename("foo.py", "api"), "api_foo_acl.py");
    }
}
