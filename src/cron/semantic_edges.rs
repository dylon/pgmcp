//! Cron job: within-project semantic file→file edges (graph-roadmap Phase 3.1).
//!
//! For each project, probes the HNSW chunk index to find, per chunk, its top
//! nearest neighbors in OTHER files of the same project; aggregates chunk pairs
//! above `semantic_edge_threshold` to file pairs (MAX cosine), caps each source
//! file to its top `semantic_edge_fanout` targets, and materializes the result
//! as **symmetric** `edge_type='semantic'` rows in `code_graph_edges`.
//!
//! These edges blend automatically into the graph-analysis cron's PageRank /
//! betweenness / community detection — `load_graph_edges` loads every edge_type
//! and `build_graph` maps `"semantic"` → `EdgeType::Semantic` — adding topical
//! affinity alongside structural (`import`) and historical (`co_change`)
//! coupling. The fan-out cap is what keeps semantic hubs from forming
//! near-cliques that would wash out modularity.
//!
//! Mirrors the cross-project similarity scanner's HNSW-LATERAL pattern, but
//! scoped WITHIN each project and aggregated to the file level. Idempotent: a
//! re-run drops the project's prior `'semantic'` edges before recomputing, so
//! the scan never double-counts and never touches other edge types.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::config::CronConfig;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::db::queries::{self, SemanticFileEdge};
use crate::stats::tracker::StatsTracker;

/// Run a full semantic-edge materialization pass over every project.
pub async fn run_semantic_edges(
    db: &dyn DbClient,
    config: &CronConfig,
    ef_search: i32,
    stats: &Arc<StatsTracker>,
    lifecycle: &DaemonLifecycle,
) {
    let pool = db
        .pool()
        .expect("semantic_edges requires a real &PgPool — DbClient backend must be PgPool-backed");
    let threshold = config.semantic_edge_threshold;
    let per_chunk_k = config.semantic_edge_per_chunk_k;
    let fanout_k = config.semantic_edge_fanout;

    info!(
        threshold,
        per_chunk_k, fanout_k, "Starting semantic-edges scan"
    );

    // Promoted to top-of-body: pairs with `semantic_edge_noop_returns` to
    // distinguish "ran, no projects" from "never ran".
    stats.semantic_edge_runs.fetch_add(1, Ordering::Relaxed);

    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for semantic-edges: {}", e);
                return;
            }
        };

    if projects.is_empty() {
        stats
            .semantic_edge_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Semantic-edges: no projects to analyze");
        return;
    }

    let start = std::time::Instant::now();
    let mut total_edges: u64 = 0;
    for (project_id, project_name) in &projects {
        if lifecycle.is_stopping() {
            info!("semantic-edges: lifecycle stopping, breaking project loop");
            break;
        }
        match analyze_project(
            pool,
            *project_id,
            project_name,
            threshold,
            per_chunk_k,
            fanout_k,
            ef_search,
        )
        .await
        {
            Ok(n) => total_edges += n,
            Err(e) => error!(
                project = %project_name,
                error = %e,
                "Semantic-edges failed for project"
            ),
        }
    }

    // `semantic_edge_runs` was promoted to top-of-body above.
    stats
        .semantic_edges_found
        .store(total_edges, Ordering::Relaxed);
    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        edges = total_edges,
        "Semantic-edges scan complete"
    );
}

#[allow(clippy::too_many_arguments)]
async fn analyze_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    threshold: f64,
    per_chunk_k: i32,
    fanout_k: i32,
    ef_search: i32,
) -> Result<u64, sqlx::Error> {
    let start = std::time::Instant::now();

    // Idempotent rebuild: drop the project's prior semantic edges first.
    let deleted = queries::delete_semantic_edges_for_project(pool, project_id).await?;

    let directed = queries::compute_semantic_file_edges(
        pool,
        project_id,
        threshold,
        per_chunk_k,
        fanout_k,
        ef_search,
    )
    .await?;

    if directed.is_empty() {
        info!(
            project = %project_name,
            deleted, "Semantic-edges: no qualifying pairs for project"
        );
        return Ok(0);
    }

    let edges = symmetrize_edges(&directed);

    let inserted = queries::bulk_insert_semantic_edges(pool, project_id, &edges).await?;

    info!(
        project = %project_name,
        deleted,
        directed = directed.len(),
        inserted,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "Semantic-edges complete for project"
    );
    Ok(inserted)
}

/// Make a directed set of file→file edges symmetric: semantic affinity is
/// undirected, so for every `a→b` ensure `b→a` exists too (file A may be in
/// B's top-fanout without the reverse holding). De-duplicates and keeps the
/// max weight per ordered pair, so the single bulk INSERT has no duplicate
/// `ON CONFLICT` targets (`… cannot affect a row a second time`). Self-loops
/// (`a == b`) are dropped — they carry no graph signal.
fn symmetrize_edges(directed: &[SemanticFileEdge]) -> Vec<SemanticFileEdge> {
    let mut sym: HashMap<(i64, i64), f64> = HashMap::with_capacity(directed.len() * 2);
    for e in directed {
        if e.source_file_id == e.target_file_id {
            continue;
        }
        for (s, t) in [
            (e.source_file_id, e.target_file_id),
            (e.target_file_id, e.source_file_id),
        ] {
            sym.entry((s, t))
                .and_modify(|w| {
                    if e.weight > *w {
                        *w = e.weight;
                    }
                })
                .or_insert(e.weight);
        }
    }
    sym.into_iter()
        .map(|((s, t), w)| SemanticFileEdge {
            source_file_id: s,
            target_file_id: t,
            weight: w,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(s: i64, t: i64, w: f64) -> SemanticFileEdge {
        SemanticFileEdge {
            source_file_id: s,
            target_file_id: t,
            weight: w,
        }
    }

    #[test]
    fn symmetrize_adds_reverse_and_dedups_keeping_max() {
        // 1→2 @0.9 and the reverse 2→1 @0.8 already present: both directions
        // must survive, each carrying the MAX weight seen for that ordered pair.
        let directed = vec![edge(1, 2, 0.9), edge(2, 1, 0.8), edge(1, 3, 0.7)];
        let mut out = symmetrize_edges(&directed);
        out.sort_by(|a, b| {
            (a.source_file_id, a.target_file_id).cmp(&(b.source_file_id, b.target_file_id))
        });
        // Pairs: (1,2),(2,1),(1,3),(3,1) — four ordered edges, no dups.
        assert_eq!(out.len(), 4, "got {out:?}");
        let w = |s: i64, t: i64| {
            out.iter()
                .find(|e| e.source_file_id == s && e.target_file_id == t)
                .map(|e| e.weight)
        };
        // (1,2) and (2,1) both take the max(0.9, 0.8) = 0.9.
        assert_eq!(w(1, 2), Some(0.9));
        assert_eq!(w(2, 1), Some(0.9));
        // (1,3) mirrored to (3,1), both 0.7.
        assert_eq!(w(1, 3), Some(0.7));
        assert_eq!(w(3, 1), Some(0.7));
    }

    #[test]
    fn symmetrize_drops_self_loops_and_handles_empty() {
        assert!(symmetrize_edges(&[]).is_empty());
        assert!(
            symmetrize_edges(&[edge(5, 5, 0.99)]).is_empty(),
            "self-loops carry no graph signal"
        );
    }
}
