//! `tool_code_ppr_search` — graph-aware code retrieval via Personalized
//! PageRank (HippoRAG; Gutiérrez et al. 2024), ported from the memory server's
//! `memory_ppr_search` to the **code** graph. (graph-roadmap Phase 3.3)
//!
//! Flat cosine retrieval returns the chunks most textually similar to the
//! query but misses code that is *structurally* related — the caller, the
//! config it reads, the type it returns. Code-PPR fixes that in one shot:
//!
//! 1. Embed the query; take the top dense-similar chunks → their files = seeds.
//! 2. Load the project's full dependency graph (import / call / co_change /
//!    semantic edges) and run Personalized PageRank restarting on the seeds.
//! 3. Return the highest-PageRank files' best-matching chunks.
//!
//! The walk pulls in files one or two hops from the lexical hits — exactly the
//! relational neighborhood ("how does X flow to Y") that flat cosine can't
//! reach — saving the agent N follow-up `read_file` / `grep` round-trips.
//! Query-time but bounded: one indexed graph load + a 25-iteration sparse
//! power-iteration.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::graph::algorithms_ext::personalized_pagerank;
use crate::mcp::server::CodePprSearchParams;
use crate::mcp::tools::fix_helpers::load_code_graph_all_edges;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

/// PPR power-iteration budget (matches the memory server's HippoRAG impl).
const PPR_MAX_ITERS: usize = 25;
const PPR_TOLERANCE: f64 = 1e-6;

pub async fn tool_code_ppr_search(
    ctx: &SystemContext,
    params: CodePprSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "code_ppr_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let project_id = project_id_or_err(ctx, &params.project).await?;
    let alpha = params.alpha.unwrap_or(0.85);
    if !(0.0..=1.0).contains(&alpha) {
        return Err(McpError::invalid_params("alpha must be in [0,1]", None));
    }
    let max_seeds = params.max_seeds.unwrap_or(10).clamp(1, 100);
    let k = params.k.unwrap_or(10).clamp(1, 100);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("no database pool", None))?;
    let ef = ctx.config().load().vector.ef_search;

    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;

    // 1. Dense seed files.
    let seed_files = queries::ppr_seed_files(pool, &embedding, project_id, max_seeds, ef)
        .await
        .map_err(|e| McpError::internal_error(format!("seed query failed: {}", e), None))?;
    if seed_files.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "results": [],
            "guidance": "No chunks matched the query in this project (or embeddings not yet \
                         backfilled). Try `semantic_search` / `hybrid_search`."
        }));
    }

    // 2. Full dependency graph + map seed files → graph nodes.
    let bundle = load_code_graph_all_edges(ctx, project_id).await?;
    let cg = &bundle.graph;
    let mut seeds: HashMap<NodeIndex, f64> = HashMap::with_capacity(seed_files.len());
    for (fid, sim) in &seed_files {
        if let Some(&ni) = cg.file_id_to_node.get(fid) {
            seeds.insert(ni, sim.max(0.0));
        }
    }

    // 3. PPR walk (when seeds landed in the graph); else fall back to dense order
    //    so the tool still helps projects whose graph crons haven't run.
    let (ranked, ppr_iterations, ppr_converged, used_graph): (Vec<(i64, f64)>, usize, bool, bool) =
        if seeds.is_empty() {
            let mut r: Vec<(i64, f64)> = seed_files.clone();
            r.truncate(k as usize);
            (r, 0, true, false)
        } else {
            let ppr = personalized_pagerank(&cg.graph, &seeds, alpha, PPR_MAX_ITERS, PPR_TOLERANCE);
            let mut scored: Vec<(i64, f64)> = ppr
                .scores
                .iter()
                .filter_map(|(ni, score)| cg.node_to_file_id.get(ni).map(|fid| (*fid, *score)))
                .filter(|(_, s)| *s > 0.0)
                .collect();
            scored.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });
            scored.truncate(k as usize);
            (scored, ppr.iterations, ppr.converged, true)
        };

    // 4. Materialize each ranked file's best chunk for the query.
    let file_ids: Vec<i64> = ranked.iter().map(|(f, _)| *f).collect();
    let chunks = queries::best_chunk_per_file(pool, &embedding, &file_ids)
        .await
        .map_err(|e| McpError::internal_error(format!("chunk query failed: {}", e), None))?;
    let by_file: HashMap<i64, queries::PprFileChunk> =
        chunks.into_iter().map(|c| (c.file_id, c)).collect();

    let results: Vec<serde_json::Value> = ranked
        .iter()
        .filter_map(|(fid, score)| {
            by_file.get(fid).map(|c| {
                json!({
                    "file": c.relative_path,
                    "language": c.language,
                    "score": format!("{:.6}", score),
                    "score_kind": if used_graph { "ppr" } else { "dense_similarity" },
                    "dense_similarity": format!("{:.4}", c.similarity),
                    "lines": format!("{}-{}", c.start_line, c.end_line),
                    "chunk": c.content,
                })
            })
        })
        .collect();

    // Reuse the memory graph-RAG latency cap (same class of bounded graph walk).
    let cap_ms = ctx.config().load().memory.graph_rag.max_latency_ms;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if cap_ms > 0 && elapsed_ms as i64 > cap_ms {
        ctx.stats()
            .graph_retrieval_latency_violations
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            tool = "code_ppr_search",
            elapsed_ms,
            cap_ms,
            "code_ppr_search: latency cap exceeded"
        );
    }

    // Cross-project neighborhood (ADR-009 §4.2): PPR is scoped to this project's
    // code graph, but the developer working here often needs the code of the
    // projects it depends on (the APIs it calls) and to know who depends on it.
    // Surface that `project_depends_on` neighborhood as related projects to also
    // search (e.g. via code_raptor_search across all projects).
    let (cross_project_dependencies, cross_project_dependents) =
        crate::deps::store::cross_project_blocks(pool, project_id).await;

    json_result(&json!({
        "project": params.project,
        "seed_files": seed_files.len(),
        "graph_nodes": cg.graph.node_count(),
        "graph_edges": cg.graph.edge_count(),
        "used_graph": used_graph,
        "ppr_iterations": ppr_iterations,
        "ppr_converged": ppr_converged,
        "result_count": results.len(),
        "results": results,
        "cross_project_dependency_count": cross_project_dependencies.len(),
        "cross_project_dependencies": cross_project_dependencies,
        "cross_project_dependent_count": cross_project_dependents.len(),
        "cross_project_dependents": cross_project_dependents,
        "guidance": "Personalized PageRank over the code graph (import/call/co_change/semantic), \
            restarted on the query's dense-similar files. Ranks files by relational proximity to the \
            lexical hits — surfacing callers, callees, and config a flat cosine search would miss. \
            `used_graph=false` means no seed file had graph edges (graph crons may not have run); the \
            tool then falls back to dense order. `cross_project_dependencies` are projects this one \
            depends on — their code is not in these results; search them separately (e.g. \
            code_raptor_search with no project). Pair with `find_callers_by_signature` / `read_file` \
            to follow the strongest edges."
    }))
}
