//! `cross_project_coupling` — inter-project Martin coupling over the (now
//! multi-ecosystem) `project_dependencies` graph (ADR-027 Stage 4): per-project
//! efferent (Ce) / afferent (Ca) coupling + instability, the most-depended-upon
//! "god projects", and cross-project dependency CYCLES (SCCs of size > 1). This
//! is the direct answer to "identify dependency coupling across projects".

use std::collections::HashMap;

use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::CrossProjectCouplingParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_cross_project_coupling(
    ctx: &SystemContext,
    params: CrossProjectCouplingParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    let projects: Vec<(i32, String)> = sqlx::query_as("SELECT id, name FROM projects ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("projects: {e}"), None))?;
    // dependent → dependency (the dependent depends on the dependency).
    let edges: Vec<(i32, i32, f64)> = sqlx::query_as(
        "SELECT dependent_project_id, dependency_project_id, MAX(confidence)
           FROM project_dependencies WHERE valid_to IS NULL
            AND dependent_project_id <> dependency_project_id
          GROUP BY dependent_project_id, dependency_project_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("edges: {e}"), None))?;

    let name: HashMap<i32, String> = projects.iter().cloned().collect();
    let mut g = DiGraph::<i32, f64>::new();
    let mut idx: HashMap<i32, NodeIndex> = HashMap::new();
    for (id, _) in &projects {
        idx.entry(*id).or_insert_with(|| g.add_node(*id));
    }
    let mut ce: HashMap<i32, usize> = HashMap::new();
    let mut ca: HashMap<i32, usize> = HashMap::new();
    for (dependent, dependency, conf) in &edges {
        let a = *idx
            .entry(*dependent)
            .or_insert_with(|| g.add_node(*dependent));
        let b = *idx
            .entry(*dependency)
            .or_insert_with(|| g.add_node(*dependency));
        g.add_edge(a, b, *conf);
        *ce.entry(*dependent).or_default() += 1; // efferent: deps this project has
        *ca.entry(*dependency).or_default() += 1; // afferent: dependents on this project
    }

    // Per-project coupling, god-projects first (highest afferent coupling).
    let mut rows: Vec<(String, usize, usize, f64)> = projects
        .iter()
        .map(|(id, nm)| {
            let e = *ce.get(id).unwrap_or(&0);
            let a = *ca.get(id).unwrap_or(&0);
            let instability = if e + a > 0 {
                e as f64 / (e + a) as f64
            } else {
                0.0
            };
            (nm.clone(), e, a, instability)
        })
        .collect();
    rows.sort_by(|x, y| y.2.cmp(&x.2).then_with(|| x.0.cmp(&y.0)));
    let project_rows: Vec<_> = rows
        .iter()
        .take(limit as usize)
        .map(|(nm, e, a, i)| {
            json!({"project": nm, "efferent_coupling": e, "afferent_coupling": a, "instability": i})
        })
        .collect();

    // Cross-project cycles: strongly-connected components of size > 1.
    let cycles: Vec<Vec<String>> = tarjan_scc(&g)
        .into_iter()
        .filter(|scc| scc.len() > 1)
        .map(|scc| {
            scc.iter()
                .map(|n| name.get(&g[*n]).cloned().unwrap_or_default())
                .collect()
        })
        .collect();

    json_result(&json!({
        "project_count": projects.len(),
        "edge_count": edges.len(),
        "cross_project_cycle_count": cycles.len(),
        "cross_project_cycles": cycles,
        "projects": project_rows,
        "guidance": if edges.is_empty() {
            Some("no inter-project edges — run the project-deps cron (trigger_cron job=\"project-deps-index\"); multi-ecosystem manifests (npm/pypi/go/maven/lake) are now parsed")
        } else { None },
    }))
}
