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
use tracing::{debug, info, warn};

use crate::fcm::{self, BackendChoice, FcmBackend, GpuPrecision};
use crate::llm::LlmExtractor;
use crate::stats::tracker::StatsTracker;

/// Minimum observations per scope to bother building a tree. Below this
/// the query overhead dwarfs the value of the summarization.
pub const MIN_OBSERVATIONS_PER_SCOPE: i64 = 8;
/// Hard cap on observations sampled per scope to keep the FCM run
/// bounded; oldest are dropped by `ORDER BY created_at DESC`.
pub const MAX_OBSERVATIONS_PER_SCOPE: i64 = 800;
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
            warn!(error = %e, "memory-raptor cron: build failed");
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
                warn!(error = %e, scope_id, "raptor: per-scope build failed");
                stats
                    .memory_raptor_build_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
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

    let k = ((n as f64 / 8.0).sqrt().ceil() as usize).clamp(3, 24);
    if k > n {
        return Ok(0);
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

    // Assign each observation to its argmax cluster; gather text
    // contexts to summarize per cluster.
    let mut clusters: Vec<Vec<String>> = vec![Vec::new(); k];
    let mut cluster_ids: Vec<Vec<i64>> = vec![Vec::new(); k];
    for i in 0..n {
        let mut argmax = 0;
        let mut best = result.membership[[i, 0]];
        for j in 1..k {
            if result.membership[[i, j]] > best {
                best = result.membership[[i, j]];
                argmax = j;
            }
        }
        clusters[argmax].push(texts[i].clone());
        cluster_ids[argmax].push(ids[i]);
    }

    // Emit one level-1 summary per non-empty cluster.
    let mut written = 0_usize;
    for (j, cluster) in clusters.iter().enumerate() {
        if cluster.is_empty() {
            continue;
        }
        // Summarize via the LLM extractor's reflect path. Caps the
        // observation count fed in to avoid prompt bloat.
        let trimmed: Vec<String> = cluster.iter().take(20).cloned().collect();
        let summary_entities = match tokio::task::block_in_place(|| extractor.reflect(&trimmed)) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, scope_id, cluster = j, "raptor: cluster reflect failed");
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
            .unwrap_or_else(|| format!("Cluster {} ({} observations)", j, cluster.len()));

        // Compute the cluster centroid in the embedding space — that's
        // what the query path searches against.
        let centroid_row = result.centroids.row(j);
        let centroid_vec: Vec<f32> = centroid_row.iter().copied().collect();
        // L2-normalize so dot product == cosine.
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

    debug!(
        scope_id,
        level_1_written = written,
        "raptor: scope build complete"
    );
    Ok(written)
}
