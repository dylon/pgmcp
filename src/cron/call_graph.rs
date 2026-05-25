//! Cron job: symbol-resolved call graph (SOTA Phase 1, G2).
//!
//! For each project, reads `symbol_references` rows with `ref_kind='call'`,
//! materializes them into `code_graph_edges` rows with `edge_type='call'`,
//! then builds an in-process `CallGraph` to compute `fan_in` / `fan_out`
//! per function which is upserted back into `function_metrics`.
//!
//! Sequenced after `function-metrics` (which guarantees `function_metrics`
//! rows exist for the symbols whose fan-in/fan-out we update) and after
//! `symbol-extraction` (which produces the `symbol_references` rows).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info};

use crate::db::DbClient;
use crate::db::queries;
use crate::graph::call_graph::{CallGraph, FunctionNode, RawCallEdge};
use crate::stats::tracker::StatsTracker;

/// Brandes betweenness is parallelized via the WorkPool, but harmonic
/// centrality (and the sequential betweenness fallback when no pool is
/// available) is O(V·E) with no parallel variant. Skip those two on call
/// graphs larger than this to keep the Low-priority cron bounded; skipped
/// functions keep the 0.0 column default.
const DENSE_CENTRALITY_MAX_NODES: usize = 8000;

pub async fn run_call_graph(
    db: &dyn DbClient,
    stats: &Arc<StatsTracker>,
    work_pool: Option<Arc<crate::work_pool::pool::WorkPool>>,
) {
    let pool = db
        .pool()
        .expect("call_graph requires a real &PgPool — DbClient backend must be PgPool-backed");
    info!("Starting call-graph cron job");
    let start = std::time::Instant::now();

    // Promoted to top-of-body: pairs with `call_graph_noop_returns` to
    // distinguish "ran, no projects" from "never ran".
    stats.call_graph_runs.fetch_add(1, Ordering::Relaxed);

    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for call-graph: {}", e);
                return;
            }
        };

    if projects.is_empty() {
        stats
            .call_graph_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Call-graph cron: no projects to analyze");
        return;
    }

    let mut total_edges: u64 = 0;
    let mut total_functions: u64 = 0;

    for (project_id, project_name) in &projects {
        match analyze_project(pool, *project_id, project_name, work_pool.as_ref()).await {
            Ok(per_project) => {
                total_edges += per_project.edges_inserted;
                total_functions += per_project.functions_updated;
            }
            Err(e) => {
                error!(
                    project = %project_name,
                    error = %e,
                    "Call-graph failed for project"
                );
            }
        }
    }

    // `call_graph_runs` was promoted to top-of-body above.
    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        edges_inserted = total_edges,
        functions_updated = total_functions,
        "Call-graph cron job complete"
    );
}

#[derive(Default)]
struct CallGraphStats {
    edges_inserted: u64,
    functions_updated: u64,
}

async fn analyze_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    work_pool: Option<&Arc<crate::work_pool::pool::WorkPool>>,
) -> Result<CallGraphStats, sqlx::Error> {
    let start = std::time::Instant::now();

    // Step 1: delete prior call edges so re-runs are idempotent.
    let deleted = queries::delete_call_edges_for_project(pool, project_id).await?;

    // Step 2: read raw call edges from symbol_references.
    let raws = queries::list_call_edges_for_project(pool, project_id).await?;

    if raws.is_empty() {
        info!(
            project = %project_name,
            deleted = deleted,
            "Call-graph: no call references for project"
        );
        queries::set_call_graph_watermark(pool, project_id, Utc::now()).await?;
        return Ok(CallGraphStats::default());
    }

    // Step 3: bulk insert into code_graph_edges with edge_type='call'.
    let inserted = queries::bulk_insert_call_edges(pool, project_id, &raws).await?;

    // Step 4: fetch function nodes and build in-memory CallGraph.
    let node_rows = queries::list_function_nodes_for_project(pool, project_id).await?;
    let nodes: Vec<FunctionNode> = node_rows
        .into_iter()
        .map(|n| FunctionNode {
            symbol_id: n.symbol_id,
            file_id: n.file_id,
            name: n.name,
            relative_path: n.relative_path,
            language: n.language,
            is_method: n.parent_id.is_some(),
        })
        .collect();

    let edges: Vec<RawCallEdge> = raws
        .iter()
        .filter_map(|r| {
            r.source_symbol_id.map(|src| RawCallEdge {
                source_symbol_id: src,
                target_symbol_id: r.target_symbol_id,
                target_raw: r.target_raw.clone(),
                // Phase 4.1: weight the edge by its resolution confidence (with a
                // small floor so low-confidence edges still participate) — so
                // PageRank / betweenness / Louvain over the call graph discount
                // ambiguous bare-name guesses instead of treating every edge as
                // certain.
                weight: r.resolution_confidence.unwrap_or(0.5).max(0.05),
            })
        })
        .collect();

    let graph = CallGraph::build(nodes, edges);

    // Step 5: compute fan_in/fan_out per symbol_id and persist into function_metrics.
    let fan_in = graph.fan_in_per_function();
    let fan_out = graph.fan_out_per_function();
    let mut triples: Vec<(i64, i32, i32)> = Vec::with_capacity(graph.node_count());
    for sym_id in graph.symbol_to_node.keys() {
        let fi = fan_in.get(sym_id).copied().unwrap_or(0) as i32;
        let fo = fan_out.get(sym_id).copied().unwrap_or(0) as i32;
        triples.push((*sym_id, fi, fo));
    }
    let updated = queries::update_function_fan_io(pool, &triples).await?;

    // Step 6: function-level centralities over the call graph (graph-roadmap
    // Phase 1.1). The genericized PageRank / Louvain / k-core algorithms are
    // ~O(E) and always run; Brandes betweenness is O(V·E) but parallelized via
    // the WorkPool; harmonic centrality (no parallel variant) and the
    // sequential betweenness fallback are gated by DENSE_CENTRALITY_MAX_NODES.
    let n_nodes = graph.node_count();
    let pagerank = graph.pagerank(0.85, 100, 1e-8);
    let (communities, modularity) = graph.louvain(1.0);
    let coreness = graph.kcore();
    let betweenness = if work_pool.is_some() || n_nodes <= DENSE_CENTRALITY_MAX_NODES {
        graph.betweenness(work_pool)
    } else {
        std::collections::HashMap::new()
    };
    let harmonic = if n_nodes <= DENSE_CENTRALITY_MAX_NODES {
        graph.harmonic()
    } else {
        std::collections::HashMap::new()
    };
    if n_nodes > DENSE_CENTRALITY_MAX_NODES {
        info!(
            project = %project_name,
            nodes = n_nodes,
            cap = DENSE_CENTRALITY_MAX_NODES,
            parallel_betweenness = work_pool.is_some(),
            "Call-graph: graph exceeds dense-centrality cap; harmonic (and \
             sequential-fallback betweenness) skipped to bound the cron"
        );
    }

    let mut centrality_rows: Vec<(i64, f64, f64, i32, i32, f64)> = Vec::with_capacity(n_nodes);
    for sym_id in graph.symbol_to_node.keys() {
        let pr = pagerank.get(sym_id).copied().unwrap_or(0.0);
        let bt = betweenness.get(sym_id).copied().unwrap_or(0.0);
        let comm = communities.get(sym_id).map(|&c| c as i32).unwrap_or(-1);
        let core = coreness.get(sym_id).copied().unwrap_or(0) as i32;
        let harm = harmonic.get(sym_id).copied().unwrap_or(0.0);
        centrality_rows.push((*sym_id, pr, bt, comm, core, harm));
    }
    let centralities_updated =
        queries::update_function_centralities(pool, &centrality_rows).await?;

    queries::set_call_graph_watermark(pool, project_id, Utc::now()).await?;

    info!(
        project = %project_name,
        nodes = n_nodes,
        edges_resolved = graph.edge_count(),
        edges_inserted = inserted,
        functions_updated = updated,
        centralities_updated = centralities_updated,
        modularity = modularity,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "Call-graph complete for project"
    );

    Ok(CallGraphStats {
        edges_inserted: inserted,
        functions_updated: updated,
    })
}
