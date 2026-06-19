//! Memory-server Phase 6.1: RAPTOR summary-tree build cron.
//!
//! For each scope, clusters the level-0 observations by embedding
//! similarity (cosine via dot product on L2-normalized vectors) and
//! emits a level-1 summary per cluster. Re-uses pgmcp's existing CUDA FCM
//! machinery for clustering; the LLM extractor handles summarization.
//!
//! Idempotency: the cron deletes the prior `memory_summary_tree` rows
//! for the scope before re-building. This is acceptable because the
//! tree is regenerated on each tick (no incremental update path) and
//! `RAPTOR` queries don't hold references across builds.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use ndarray::Array2;
use pgvector::Vector;
use sqlx::PgPool;
use tracing::{debug, error, info};

use crate::fcm::{self, BackendChoice, FcmBackend, GpuPrecision};
use crate::llm::LlmExtractor;
use crate::stats::tracker::StatsTracker;

/// Minimum observations per scope to bother building a tree. Below this
/// the query overhead dwarfs the value of the summarization.
pub const MIN_OBSERVATIONS_PER_SCOPE: i64 = 8;
/// Hard cap on observations sampled per scope to keep the FCM run
/// bounded; oldest are dropped by `ORDER BY created_at DESC`.
pub const MAX_OBSERVATIONS_PER_SCOPE: i64 = 800;
/// Cap on embedded unified-graph nodes fed into the GLOBAL RAPTOR tree
/// (Stage 6). Bounds the FCM cost; the highest-`importance` nodes win.
pub const MAX_UNIFIED_NODES: i64 = 4000;
/// Membership-degree cap. K clusters = √(N / min_cluster_size) clamped
/// to [3, 24]. Matches the `topic-clustering` cron's heuristic.
pub const FCM_FUZZINESS: f64 = 1.7;
pub const FCM_MAX_ITERS: usize = 60;
pub const FCM_TOLERANCE: f64 = 1e-4;

pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    extractor: Arc<dyn LlmExtractor>,
) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match run_raptor_build(&pool, &stats, extractor.as_ref()).await {
        Ok(scopes) => {
            info!(
                scopes_processed = scopes,
                "memory-raptor cron: tree-build complete"
            );
        }
        Err(e) => {
            error!(error = %e, "memory-raptor cron: build failed");
            stats
                .memory_raptor_build_errors
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Iterate scopes with enough observations and rebuild their summary
/// tree. Returns the number of scopes successfully rebuilt.
pub async fn run_raptor_build(
    pool: &PgPool,
    stats: &StatsTracker,
    extractor: &dyn LlmExtractor,
) -> Result<usize> {
    stats
        .memory_raptor_build_runs
        .fetch_add(1, Ordering::Relaxed);
    let scopes: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT es.scope_id
         FROM memory_entity_scope es
         JOIN memory_observations o ON o.entity_id = es.entity_id
         WHERE o.valid_to IS NULL AND o.embedding IS NOT NULL
         GROUP BY es.scope_id
         HAVING COUNT(o.id) >= $1
         ORDER BY es.scope_id",
    )
    .bind(MIN_OBSERVATIONS_PER_SCOPE)
    .fetch_all(pool)
    .await
    .context("scope discovery")?;

    let mut done = 0_usize;
    for scope_id in scopes {
        match build_scope_tree(pool, stats, extractor, scope_id).await {
            Ok(written) => {
                stats
                    .memory_raptor_summaries_written
                    .fetch_add(written as u64, Ordering::Relaxed);
                done += 1;
            }
            Err(e) => {
                error!(error = %e, scope_id, "raptor: per-scope build failed");
                stats
                    .memory_raptor_build_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Stage 6: also build the GLOBAL unified-graph summary tree spanning every
    // embedded node type — so `memory_raptor_search` returns ontology-wide
    // thematic summaries, not just per-scope observation clusters.
    match build_unified_tree(pool, extractor).await {
        Ok(w) => {
            stats
                .memory_raptor_summaries_written
                .fetch_add(w as u64, Ordering::Relaxed);
            if w > 0 {
                done += 1;
            }
        }
        Err(e) => {
            error!(error = %e, "raptor: unified-graph tree build failed");
            stats
                .memory_raptor_build_errors
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(done)
}

async fn build_scope_tree(
    pool: &PgPool,
    _stats: &StatsTracker,
    extractor: &dyn LlmExtractor,
    scope_id: i64,
) -> Result<usize> {
    let rows: Vec<(i64, String, Option<Vector>)> = sqlx::query_as(
        "SELECT o.id, o.content, o.embedding
         FROM memory_observations o
         JOIN memory_entity_scope es ON es.entity_id = o.entity_id
         WHERE es.scope_id = $1
           AND o.valid_to IS NULL
           AND o.embedding IS NOT NULL
         ORDER BY o.created_at DESC
         LIMIT $2",
    )
    .bind(scope_id)
    .bind(MAX_OBSERVATIONS_PER_SCOPE)
    .fetch_all(pool)
    .await
    .context("observation gather")?;
    if rows.is_empty() {
        return Ok(0);
    }

    let n = rows.len();
    let d: usize = 1024;
    let mut data = Array2::<f32>::zeros((n, d));
    let mut texts: Vec<String> = Vec::with_capacity(n);
    let mut ids: Vec<i64> = Vec::with_capacity(n);
    for (i, (id, content, emb)) in rows.iter().enumerate() {
        ids.push(*id);
        texts.push(content.clone());
        if let Some(v) = emb {
            let slice = v.as_slice();
            for (j, value) in slice.iter().take(d).enumerate() {
                data[[i, j]] = *value;
            }
        }
    }

    // Clear existing rows for this scope (idempotent rebuild).
    sqlx::query("DELETE FROM memory_summary_tree WHERE scope_id = $1")
        .bind(scope_id)
        .execute(pool)
        .await
        .context("clear prior tree")?;

    // Level 0: leaves point at each observation. We do not store
    // summary_text / summary_embedding for leaves (CHECK constraint
    // ensures level=0 ↔ observation_id IS NOT NULL).
    for &obs_id in &ids {
        sqlx::query(
            "INSERT INTO memory_summary_tree
                (scope_id, level, parent_id, observation_id, summary_text,
                 summary_embedding, child_count)
             VALUES ($1, 0, NULL, $2, NULL, NULL, NULL)",
        )
        .bind(scope_id)
        .bind(obs_id)
        .execute(pool)
        .await
        .context("level-0 leaf insert")?;
    }

    // Level-1 summaries via the shared cluster-and-summarize helper.
    let written = write_cluster_summaries(pool, extractor, scope_id, &texts, &data).await?;
    debug!(
        scope_id,
        level_1_written = written,
        "raptor: scope build complete"
    );
    Ok(written)
}

/// Shared RAPTOR step: FCM-cluster the `data` embeddings (rows aligned with
/// `texts`), LLM-summarize each cluster (via the extractor's `reflect`), and
/// write one level-1 summary row per non-empty cluster under `scope_id` — with
/// the L2-normalized cluster centroid as `summary_embedding` (what the query
/// path searches against). Returns the number of summaries written. Used by
/// both the per-scope observation tree and the global unified-graph tree
/// (Stage 6).
async fn write_cluster_summaries(
    pool: &PgPool,
    extractor: &dyn LlmExtractor,
    scope_id: i64,
    texts: &[String],
    data: &Array2<f32>,
) -> Result<usize> {
    let n = texts.len();
    let k = ((n as f64 / 8.0).sqrt().ceil() as usize).clamp(3, 24);
    if k > n {
        return Ok(0);
    }

    // Cluster with FCM. CUDA is mandatory for production pgmcp; surface init
    // failures instead of silently falling back to CPU.
    let mut backend: Box<dyn FcmBackend> =
        fcm::make_backend(data.clone(), k, BackendChoice::Cuda(GpuPrecision::Fp32))
            .map_err(|e| anyhow::anyhow!("fcm backend init: {}", e))?;
    let result = fcm::run_seeded(
        backend.as_mut(),
        data.view(),
        k,
        FCM_FUZZINESS,
        FCM_MAX_ITERS,
        FCM_TOLERANCE,
        None,
        None,
        Some(42),
    )
    .map_err(|e| anyhow::anyhow!("fcm run: {}", e))?;
    debug!(
        scope_id,
        n,
        k,
        iters = result.iterations,
        converged = result.converged,
        "raptor: clustering complete"
    );

    // Assign each item to its argmax cluster; gather text contexts per cluster.
    // `i` also indexes the 2-D membership matrix, so iterate `texts` by
    // enumeration (keeps `i` for `membership` while satisfying needless_range_loop).
    let mut clusters: Vec<Vec<String>> = vec![Vec::new(); k];
    for (i, text) in texts.iter().enumerate() {
        let mut argmax = 0;
        let mut best = result.membership[[i, 0]];
        for j in 1..k {
            if result.membership[[i, j]] > best {
                best = result.membership[[i, j]];
                argmax = j;
            }
        }
        clusters[argmax].push(text.clone());
    }

    // Emit one level-1 summary per non-empty cluster.
    let mut written = 0_usize;
    for (j, cluster) in clusters.iter().enumerate() {
        if cluster.is_empty() {
            continue;
        }
        // Summarize via the LLM extractor's reflect path. Cap the items fed in
        // to avoid prompt bloat.
        let trimmed: Vec<String> = cluster.iter().take(20).cloned().collect();
        let summary_entities = match tokio::task::block_in_place(|| extractor.reflect(&trimmed)) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, scope_id, cluster = j, "raptor: cluster reflect failed");
                continue;
            }
        };
        let summary_text = summary_entities
            .first()
            .map(|e| {
                let head = e.initial_observations.first().cloned().unwrap_or_default();
                if head.is_empty() {
                    e.name.clone()
                } else {
                    format!("{}: {}", e.name, head)
                }
            })
            .unwrap_or_else(|| format!("Cluster {} ({} items)", j, cluster.len()));

        // Cluster centroid in embedding space; L2-normalize so dot == cosine.
        let centroid_row = result.centroids.row(j);
        let centroid_vec: Vec<f32> = centroid_row.iter().copied().collect();
        let norm: f32 = centroid_vec
            .iter()
            .map(|v| v * v)
            .sum::<f32>()
            .sqrt()
            .max(1e-12);
        let normalized: Vec<f32> = centroid_vec.iter().map(|v| v / norm).collect();
        let centroid_vector = Vector::from(normalized);

        sqlx::query(
            "INSERT INTO memory_summary_tree
                (scope_id, level, parent_id, observation_id, summary_text,
                 summary_embedding, child_count)
             VALUES ($1, 1, NULL, NULL, $2, $3, $4)",
        )
        .bind(scope_id)
        .bind(&summary_text)
        .bind(&centroid_vector)
        .bind(cluster.len() as i32)
        .execute(pool)
        .await
        .context("level-1 summary insert")?;
        written += 1;
    }
    Ok(written)
}

/// Stage 6 — RAPTOR over the **full unified graph**. Builds a GLOBAL summary
/// tree over *all* embedded unified-graph nodes (observations, chunks,
/// work_items, experiments, commit_chunks, pattern_chunks, mandates — every
/// node type carrying an embedding), so `memory_raptor_search` returns thematic
/// summaries spanning the whole ontology, not just per-scope observations.
/// Stored under a dedicated `__unified_graph__` scope as summary-only rows
/// (no level-0 leaves), which satisfies the summary-tree CHECK without a schema
/// change. Reuses [`write_cluster_summaries`].
async fn build_unified_tree(pool: &PgPool, extractor: &dyn LlmExtractor) -> Result<usize> {
    let scope_id = crate::db::queries::find_or_create_scope(
        pool,
        &crate::db::queries::ScopeSpec {
            user_id: None,
            agent_id: Some("__unified_graph__".to_string()),
            session_id: None,
            project_id: None,
        },
    )
    .await
    .context("unified scope")?;

    let rows: Vec<(String, Option<Vector>)> = sqlx::query_as(
        "SELECT label, embedding
         FROM memory_unified_nodes
         WHERE embedding IS NOT NULL
         ORDER BY importance DESC
         LIMIT $1",
    )
    .bind(MAX_UNIFIED_NODES)
    .fetch_all(pool)
    .await
    .context("unified-node gather")?;
    if (rows.len() as i64) < MIN_OBSERVATIONS_PER_SCOPE {
        return Ok(0);
    }

    let n = rows.len();
    let mut data = Array2::<f32>::zeros((n, 1024));
    let mut texts: Vec<String> = Vec::with_capacity(n);
    for (i, (label, emb)) in rows.iter().enumerate() {
        texts.push(label.clone());
        if let Some(v) = emb {
            for (col, value) in v.as_slice().iter().take(1024).enumerate() {
                data[[i, col]] = *value;
            }
        }
    }

    sqlx::query("DELETE FROM memory_summary_tree WHERE scope_id = $1")
        .bind(scope_id)
        .execute(pool)
        .await
        .context("clear prior unified tree")?;
    write_cluster_summaries(pool, extractor, scope_id, &texts, &data).await
}
