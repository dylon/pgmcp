//! Topic-clustering bake-off (Phase 3) — the empirical experiment.
//!
//! The user asked to "experiment with both [engines] to see how they perform
//! against the actual embeddings." This harness runs every engine on the SAME
//! per-project input with the SAME fixed K (a fair control) and scores them with
//! the identical [`TopicMetrics`] suite, then writes a scientific-ledger
//! markdown comparison and picks a per-project + overall winner.
//!
//! Engines compared:
//! - **baseline** — FCM on raw 1024-d embeddings (quantifies the collapse);
//! - **embedding_pca** — FCM on PCA-reduced embeddings (Track B, SOTA);
//! - **embedding_rp** — FCM on JL-random-projection-reduced embeddings;
//! - **graph** — Louvain communities over the fused semantic+import+co_change
//!   graph (Track A, novel).
//!
//! It is read-only w.r.t. the topic tables (it never persists topics); it only
//! writes the ledger file. Run via `pgmcp analyze topic-bakeoff`.

use std::sync::Arc;

use tracing::{error, info};

use crate::config::CronConfig;
use crate::cron::topic_clustering::{
    ClusteringSummary, TopicEngine, cluster_embeddings_engine, estimate_k,
};
use crate::cron::topic_graph;
use crate::db::DbClient;
use crate::stats::tracker::StatsTracker;

/// Default representative project set spanning size + domain (code / prose /
/// giant). Overridable via `PGMCP_BAKEOFF_PROJECTS` (comma-separated).
const DEFAULT_PROJECTS: &[&str] = &[
    "liblevenshtein-rust", // ~18k chunks, pure code
    "mettail-rust",        // ~22k chunks, code
    "Papers",              // ~31k chunks, pure prose
    "pgmcp",               // this project, mixed code + docs
];

/// The default representative project set for the bake-off (overridable via
/// `PGMCP_BAKEOFF_PROJECTS`).
pub fn default_projects() -> &'static [&'static str] {
    DEFAULT_PROJECTS
}

/// One engine's result on one project.
struct EngineRun {
    engine: &'static str,
    summary: ClusteringSummary,
    elapsed_s: f64,
}

/// Resolve a project name → id (best-effort; None if absent).
async fn project_id(db: &dyn DbClient, name: &str) -> Option<i32> {
    let pool = db.pool()?;
    sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE name = $1 ORDER BY id LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Run the bake-off over `projects`; returns the rendered markdown report.
pub async fn run_bakeoff(
    db: &dyn DbClient,
    config: &CronConfig,
    projects: &[String],
    stats: &Arc<StatsTracker>,
) -> anyhow::Result<String> {
    let _ = stats; // reserved for future per-run counters
    let mut md = String::new();
    md.push_str("# Topic-clustering bake-off\n\n");
    md.push_str(
        "Engines scored on identical per-project input with a fixed K (fair control). \
         Higher NPMI / diversity / modularity / silhouette = better; \
         `distinct_label_ratio` near 1.0 and `topics_per_doc` near 1 are healthy; \
         a degenerate run (per the Phase-1 gate) is flagged.\n\n",
    );

    let mut overall: Vec<(String, String, f64)> = Vec::new(); // (project, winner_engine, npmi)

    for project in projects {
        info!(project, "bake-off: starting project");
        let rows = match db.bulk_extract_project_embeddings(project, None).await {
            Ok(r) => r,
            Err(e) => {
                error!(project, error = %e, "bake-off: failed to extract embeddings; skipping");
                md.push_str(&format!("## {project}\n\n_skipped: {e}_\n\n"));
                continue;
            }
        };
        if rows.is_empty() {
            md.push_str(&format!(
                "## {project}\n\n_skipped: no embedded chunks_\n\n"
            ));
            continue;
        }
        let n = rows.len();

        // Fixed K for all embedding engines (fair control; skips the K sweep).
        let fixed_k = estimate_k(n, config.topic_min_cluster_size);
        let mut eng_config = config.clone();
        eng_config.topic_num_clusters = Some(fixed_k);

        // Graph edges for the graph engine.
        let edges = match project_id(db, project).await {
            Some(pid) => topic_graph::load_project_graph_edges(db, pid)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let ew = {
            let w = &config.topic_graph_edge_weights;
            [
                w.first().copied().unwrap_or(1.0),
                w.get(1).copied().unwrap_or(1.0),
                w.get(2).copied().unwrap_or(1.0),
            ]
        };

        let mut runs: Vec<EngineRun> = Vec::new();
        for (engine_name, engine) in [
            ("baseline", TopicEngine::Baseline),
            ("embedding_pca", TopicEngine::EmbeddingPca),
            ("embedding_rp", TopicEngine::EmbeddingRp),
            ("embedding_hdbscan", TopicEngine::EmbeddingHdbscan),
        ] {
            let t0 = std::time::Instant::now();
            let summary = cluster_embeddings_engine(
                &rows,
                &eng_config,
                config.topic_min_cluster_size,
                &format!("bakeoff:{project}:{engine_name}"),
                engine,
            );
            runs.push(EngineRun {
                engine: engine_name,
                summary,
                elapsed_s: t0.elapsed().as_secs_f64(),
            });
            info!(project, engine = engine_name, "bake-off: engine done");
        }
        // Graph engine.
        {
            let t0 = std::time::Instant::now();
            let summary = topic_graph::cluster_graph(
                &rows,
                &edges,
                ew,
                config.topic_graph_resolution,
                config.topic_min_cluster_size,
                config.topic_label_top_k,
                &format!("bakeoff:{project}:graph"),
            );
            runs.push(EngineRun {
                engine: "graph",
                summary,
                elapsed_s: t0.elapsed().as_secs_f64(),
            });
            info!(project, engine = "graph", "bake-off: engine done");
        }

        // Render this project's table.
        md.push_str(&format!(
            "## {project}\n\n{n} chunks · fixed K = {fixed_k} · {} graph edges\n\n",
            edges.len()
        ));
        md.push_str(
            "| engine | topics | noise% | distinct_label | topics/doc | max_share | NPMI | diversity | silhouette | modularity | sec |\n",
        );
        md.push_str("|:──|──:|──:|──:|──:|──:|──:|──:|──:|──:|──:|\n");

        let dthr = crate::quality::topic_metrics::DegeneracyThresholds::from_config(config);
        let mut best: Option<(&'static str, f64)> = None;
        for r in &runs {
            let m = r.summary.metrics.as_ref();
            let noise_pct = if r.summary.chunks_analyzed > 0 {
                100.0 * r.summary.noise_chunks as f64 / r.summary.chunks_analyzed as f64
            } else {
                0.0
            };
            let g = |v: Option<f64>| v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "—".into());
            let npmi = m.map(|m| m.npmi_coherence);
            let degen = m.map(|m| m.degeneracy_reason(&dthr)).unwrap_or(None);
            let flag = if degen.is_some() { " ⚠" } else { "" };
            md.push_str(&format!(
                "| {}{} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {:.1} |\n",
                r.engine,
                flag,
                r.summary.topics_found,
                noise_pct,
                g(m.map(|m| m.distinct_label_ratio)),
                g(m.map(|m| m.topics_per_doc_mean)),
                g(m.map(|m| m.max_topic_share)),
                g(npmi),
                g(m.map(|m| m.topic_diversity)),
                g(m.and_then(|m| if m.fuzzy_silhouette.is_finite() {
                    Some(m.fuzzy_silhouette)
                } else {
                    None
                })),
                g(m.and_then(|m| if m.modularity.is_finite() {
                    Some(m.modularity)
                } else {
                    None
                })),
                r.elapsed_s,
            ));
            // Winner = highest NPMI among non-degenerate engines.
            if degen.is_none()
                && let Some(npmi) = npmi.filter(|v| v.is_finite())
                && best.map(|(_, b)| npmi > b).unwrap_or(true)
            {
                best = Some((r.engine, npmi));
            }
        }
        md.push('\n');
        if let Some((winner, npmi)) = best {
            md.push_str(&format!(
                "**Winner: `{winner}`** (NPMI {npmi:.3}, non-degenerate).\n\n"
            ));
            overall.push((project.clone(), winner.to_string(), npmi));
        } else {
            md.push_str("**Winner: none** (all engines degenerate or coherence unavailable).\n\n");
        }
    }

    // Overall tally.
    md.push_str("## Overall\n\n");
    if overall.is_empty() {
        md.push_str("_No project produced a non-degenerate winner._\n");
    } else {
        let mut tally: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (_p, w, _n) in &overall {
            *tally.entry(w.clone()).or_insert(0) += 1;
        }
        let mut tally: Vec<(String, usize)> = tally.into_iter().collect();
        tally.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        md.push_str("Per-project winners:\n\n");
        for (p, w, npmi) in &overall {
            md.push_str(&format!("- {p}: `{w}` (NPMI {npmi:.3})\n"));
        }
        md.push_str("\nWin counts:\n\n");
        for (w, c) in &tally {
            md.push_str(&format!("- `{w}`: {c}\n"));
        }
        md.push_str(&format!(
            "\n**Recommended default `topic_clustering_method`: `{}`**\n",
            tally
                .first()
                .map(|(w, _)| w.as_str())
                .unwrap_or("embedding_pca")
        ));
    }

    Ok(md)
}
