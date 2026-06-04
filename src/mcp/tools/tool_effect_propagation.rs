//! `tool_effect_propagation` — forward effect closure along resolved
//! call edges.
//!
//! "What touches network?", "what reaches gpu_kernel?" — answer via a
//! bounded BFS over the resolved-edge subgraph. Implementation reuses
//! the `effects_reachable_from` helper.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::EffectPropagationParams;
use crate::mcp::tools::sema_helpers::effects::{effects_reachable_from, symbols_with_any_effect};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_effect_propagation(
    ctx: &SystemContext,
    params: EffectPropagationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "effect_propagation", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let max_depth = params.max_depth.unwrap_or(8).clamp(1, 32);
    let target_effects = params.target_effects.clone();
    let limit = params.limit.unwrap_or(50).max(1) as i64;

    // Two modes:
    // 1. seed_symbol_id provided → forward reachability from that symbol.
    // 2. otherwise → find all symbols that REACH any of the target_effects.

    if let Some(seed) = params.seed_symbol_id {
        let reach = effects_reachable_from(pool, seed, max_depth)
            .await
            .unwrap_or_default();
        let entries: Vec<serde_json::Value> = reach
            .into_iter()
            .map(|(effect, stats)| {
                json!({
                    "effect": effect,
                    "reached_count": stats.count,
                    "min_depth": stats.min_depth,
                })
            })
            .collect();
        return json_result(&json!({
            "mode": "forward_from_seed",
            "seed_symbol_id": seed,
            "max_depth": max_depth,
            "reached_effects": entries,
        }));
    }

    if target_effects.is_empty() {
        return json_result(&json!({
            "mode": "reverse_to_targets",
            "results": [],
            "guidance": "Provide either seed_symbol_id (forward reachability) or target_effects (reverse propagation)."
        }));
    }

    // Reverse propagation: who reaches any of the target effects?
    // Approach: take all symbols carrying any of the target effects,
    // then BFS backward via symbol_references for `max_depth` hops.
    let leaf_symbols = symbols_with_any_effect(pool, project_id, &target_effects)
        .await
        .unwrap_or_default();
    let leaf_ids: Vec<i64> = leaf_symbols.iter().map(|(sid, _, _, _)| *sid).collect();

    if leaf_ids.is_empty() {
        return json_result(&json!({
            "mode": "reverse_to_targets",
            "target_effects": target_effects,
            "results": [],
            "guidance": "No symbols in the project carry any of the requested effects."
        }));
    }

    // Reverse BFS — at each layer, find callers (rows whose
    // target_symbol_id is in the frontier).
    use std::collections::{HashMap, HashSet};
    let mut depth_of: HashMap<i64, u32> = leaf_ids.iter().map(|&id| (id, 0u32)).collect();
    let mut frontier: HashSet<i64> = leaf_ids.iter().copied().collect();
    for depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let frontier_vec: Vec<i64> = frontier.iter().copied().collect();
        let callers: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT sr.source_symbol_id
             FROM symbol_references sr
             WHERE sr.target_symbol_id = ANY($1::int8[])
               AND sr.source_symbol_id IS NOT NULL
               AND sr.resolution_kind IN ('exact_in_file', 'exact_via_import')",
        )
        .bind(&frontier_vec)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        let mut new_frontier: HashSet<i64> = HashSet::new();
        for caller in callers {
            if let std::collections::hash_map::Entry::Vacant(e) = depth_of.entry(caller) {
                e.insert(depth);
                new_frontier.insert(caller);
            }
        }
        frontier = new_frontier;
    }

    let mut results: Vec<(i64, u32)> = depth_of.into_iter().collect();
    results.sort_by_key(|(_, d)| *d);
    results.truncate(limit as usize);
    let result_ids: Vec<i64> = results.iter().map(|(id, _)| *id).collect();

    type SymRow = (i64, String, Option<String>, String, String);
    let sym_rows: Vec<SymRow> = if result_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT fs.id, fs.name, fs.scope_path, fs.kind, f.relative_path
             FROM file_symbols fs
             JOIN indexed_files f ON f.id = fs.file_id
             WHERE fs.id = ANY($1::int8[])",
        )
        .bind(&result_ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
    };
    let mut by_id: HashMap<i64, (String, Option<String>, String, String)> = HashMap::new();
    for (sid, name, scope, kind, path) in sym_rows {
        by_id.insert(sid, (name, scope, kind, path));
    }

    let payload: Vec<serde_json::Value> = results
        .into_iter()
        .map(|(sid, depth)| {
            let (name, scope, kind, path) = by_id.get(&sid).cloned().unwrap_or_else(|| {
                (
                    "(unknown)".into(),
                    None,
                    "(unknown)".into(),
                    "(unknown)".into(),
                )
            });
            json!({
                "symbol_id": sid,
                "depth": depth,
                "name": name,
                "scope_path": scope,
                "kind": kind,
                "file": path,
            })
        })
        .collect();

    // Cross-project neighborhood (ADR-009 §4.2): these in-project effects also
    // propagate to the projects that depend on this one — surface them so an
    // agent can warn/coordinate downstream.
    let (_deps, cross_project_dependents) =
        crate::deps::store::cross_project_blocks(pool, project_id).await;

    json_result(&json!({
        "mode": "reverse_to_targets",
        "target_effects": target_effects,
        "max_depth": max_depth,
        "results": payload,
        "cross_project_dependent_count": cross_project_dependents.len(),
        "cross_project_dependents": cross_project_dependents,
        "guidance": "Returns symbols that REACH (via resolved call edges) any symbol carrying \
                     one of the target_effects, with the minimum hop count. depth=0 = direct \
                     carrier. `cross_project_dependents` are projects that depend on this one and \
                     may be affected by these effects — coordinate via a2a_active_agents."
    }))
}
