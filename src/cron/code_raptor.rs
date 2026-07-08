//! Cron job: RAPTOR-over-code summary tree (graph-roadmap Phase 3.3).
//!
//! For each project, clusters file-chunk embeddings with the existing CUDA FCM
//! machinery (the same backend the `topic-clustering` cron uses) and emits one
//! level-1 summary per cluster into `code_summary_tree`. Each summary captures a
//! "module gist" that no single chunk contains; the cluster **centroid** in
//! embedding space IS the summary's embedding (no re-embedding needed), and
//! `code_raptor_search` does cosine ANN against it for conceptual queries.
//!
//! The summary *text* is built deterministically from member file paths (top
//! directories + representative basenames) — no LLM — so the build is exact,
//! free, and reproducible. Idempotent: the prior rows for a project are deleted
//! before its tree is rebuilt.
//!
//! Uses `embedding_v2` (BGE-M3, 1024d) so the centroid matches the query
//! embedder; chunks without `embedding_v2` yet (mid-migration) are skipped.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ndarray::Array2;
use pgvector::Vector;
use sqlx::PgPool;
use tracing::{error, info};

use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::fcm::{self, BackendChoice, FcmBackend, GpuPrecision};
use crate::stats::tracker::StatsTracker;

const EMBED_DIM: usize = 1024;
const MAX_CHUNKS_PER_PROJECT: i64 = 20_000;
const FCM_FUZZINESS: f64 = 2.0;
const FCM_MAX_ITERS: usize = 100;
const FCM_TOLERANCE: f64 = 1e-5;
/// Representative member files recorded per summary.
const MAX_MEMBER_PATHS: usize = 12;

/// Run a full RAPTOR-over-code build across every project.
pub async fn run_code_raptor(
    db: &dyn DbClient,
    stats: &Arc<StatsTracker>,
    lifecycle: &DaemonLifecycle,
) {
    let pool = db
        .pool()
        .expect("code_raptor requires a real &PgPool — DbClient backend must be PgPool-backed");
    info!("Starting code-raptor cron job");
    let start = std::time::Instant::now();
    stats.code_raptor_runs.fetch_add(1, Ordering::Relaxed);

    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for code-raptor: {}", e);
                return;
            }
        };

    if projects.is_empty() {
        stats
            .code_raptor_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Code-raptor: no projects to analyze");
        return;
    }

    let mut total_summaries: u64 = 0;
    for (project_id, project_name) in &projects {
        if lifecycle.is_stopping() {
            info!("code-raptor: lifecycle stopping, breaking project loop");
            break;
        }
        match build_project_tree(pool, *project_id, project_name).await {
            Ok(n) => total_summaries += n,
            Err(e) => error!(
                project = %project_name,
                error = %e,
                "Code-raptor failed for project"
            ),
        }
    }

    stats
        .code_raptor_summaries_written
        .store(total_summaries, Ordering::Relaxed);
    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        summaries = total_summaries,
        "Code-raptor cron job complete"
    );
}

async fn build_project_tree(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
) -> Result<u64, sqlx::Error> {
    // Corpus-scale: loading up to MAX_CHUNKS_PER_PROJECT embedding vectors for a
    // project can exceed the pool's 30 s default; lift the timeout for this read.
    let mut tx = crate::db::pool::begin_heavy(pool, "120s", "code-raptor").await?;
    let rows: Vec<(String, Option<Vector>)> = sqlx::query_as(
        "SELECT f.relative_path, c.embedding_v2
         FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         WHERE f.project_id = $1 AND c.embedding_v2 IS NOT NULL
         LIMIT $2",
    )
    .bind(project_id)
    .bind(MAX_CHUNKS_PER_PROJECT)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let n = rows.len();
    // Need enough chunks to form meaningful clusters.
    if n < 8 {
        // Still clear any stale rows so a shrunk project doesn't keep ghosts.
        sqlx::query("DELETE FROM code_summary_tree WHERE project_id = $1")
            .bind(project_id)
            .execute(pool)
            .await?;
        return Ok(0);
    }

    let mut data = Array2::<f32>::zeros((n, EMBED_DIM));
    let mut paths: Vec<String> = Vec::with_capacity(n);
    for (i, (path, emb)) in rows.iter().enumerate() {
        paths.push(path.clone());
        if let Some(v) = emb {
            for (j, value) in v.as_slice().iter().take(EMBED_DIM).enumerate() {
                data[[i, j]] = *value;
            }
        }
    }

    // K = √(n/8) clamped — matches the topic-clustering / memory-raptor heuristic.
    let k = ((n as f64 / 8.0).sqrt().ceil() as usize).clamp(3, 24);
    if k > n {
        return Ok(0);
    }

    let mut backend: Box<dyn FcmBackend> =
        fcm::make_backend(data.clone(), k, BackendChoice::Cuda(GpuPrecision::Fp32))
            .map_err(|e| sqlx::Error::Protocol(format!("fcm backend init: {e}")))?;
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
    .map_err(|e| sqlx::Error::Protocol(format!("fcm run: {e}")))?;

    // Argmax cluster assignment; gather member paths per cluster.
    let mut cluster_paths: Vec<Vec<String>> = vec![Vec::new(); k];
    for (i, p) in paths.iter().enumerate() {
        let mut argmax = 0usize;
        let mut best = result.membership[[i, 0]];
        for j in 1..k {
            if result.membership[[i, j]] > best {
                best = result.membership[[i, j]];
                argmax = j;
            }
        }
        cluster_paths[argmax].push(p.clone());
    }

    // Idempotent rebuild.
    sqlx::query("DELETE FROM code_summary_tree WHERE project_id = $1")
        .bind(project_id)
        .execute(pool)
        .await?;

    let mut written = 0u64;
    for (j, members) in cluster_paths.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let summ = summarize_cluster(members);
        // Centroid → L2-normalized summary embedding (dot == cosine).
        let centroid: Vec<f32> = result.centroids.row(j).iter().copied().collect();
        let norm: f32 = centroid
            .iter()
            .map(|v| v * v)
            .sum::<f32>()
            .sqrt()
            .max(1e-12);
        let normalized: Vec<f32> = centroid.iter().map(|v| v / norm).collect();
        let embedding = Vector::from(normalized);

        sqlx::query(
            "INSERT INTO code_summary_tree
                (project_id, level, summary_text, summary_embedding,
                 member_count, member_paths, top_topics)
             VALUES ($1, 1, $2, $3, $4, $5, $6)",
        )
        .bind(project_id)
        .bind(&summ.text)
        .bind(&embedding)
        .bind(summ.member_count as i32)
        .bind(&summ.member_paths)
        .bind(&summ.top_dirs)
        .execute(pool)
        .await?;
        written += 1;
    }

    info!(
        project = %project_name,
        chunks = n,
        clusters = k,
        summaries = written,
        "Code-raptor tree built"
    );
    Ok(written)
}

/// Deterministic structural summary of a cluster's member chunk file paths.
struct ClusterSummary {
    text: String,
    member_paths: Vec<String>,
    top_dirs: Vec<String>,
    member_count: usize,
}

/// Build a deterministic, human-readable summary from a cluster's member file
/// paths: the most common directories + a sample of distinct files. Pure (no
/// DB / model) so it is unit-testable.
fn summarize_cluster(member_paths: &[String]) -> ClusterSummary {
    let member_count = member_paths.len();

    // Distinct files (a chunk's file may repeat across the cluster).
    let mut distinct: Vec<String> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for p in member_paths {
        if seen.insert(p.as_str(), ()).is_none() {
            distinct.push(p.clone());
        }
    }
    distinct.sort();

    // Directory frequencies (parent of each distinct file).
    let mut dir_freq: HashMap<String, usize> = HashMap::new();
    for p in &distinct {
        let dir = match p.rfind('/') {
            Some(i) => p[..i].to_string(),
            None => ".".to_string(),
        };
        *dir_freq.entry(dir).or_insert(0) += 1;
    }
    let mut dirs: Vec<(String, usize)> = dir_freq.into_iter().collect();
    // Most frequent first; ties broken alphabetically for determinism.
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_dirs: Vec<String> = dirs.iter().take(3).map(|(d, _)| d.clone()).collect();

    let sample: Vec<String> = distinct.iter().take(MAX_MEMBER_PATHS).cloned().collect();
    let sample_basenames: Vec<&str> = sample
        .iter()
        .take(5)
        .map(|p| p.rsplit('/').next().unwrap_or(p.as_str()))
        .collect();

    let dirs_str = if top_dirs.is_empty() {
        "(root)".to_string()
    } else {
        top_dirs.join(", ")
    };
    let text = format!(
        "Conceptual cluster over {dirs_str}: {} ({} chunks across {} files)",
        sample_basenames.join(", "),
        member_count,
        distinct.len()
    );

    ClusterSummary {
        text,
        member_paths: sample,
        top_dirs,
        member_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_picks_top_dirs_and_samples() {
        let members = vec![
            "src/graph/algorithms.rs".to_string(),
            "src/graph/algorithms.rs".to_string(), // duplicate chunk in same file
            "src/graph/dsm.rs".to_string(),
            "src/graph/pathrank.rs".to_string(),
            "src/db/queries.rs".to_string(),
        ];
        let s = summarize_cluster(&members);
        assert_eq!(s.member_count, 5);
        // src/graph appears 3× (distinct files) → top dir.
        assert_eq!(s.top_dirs.first().map(|s| s.as_str()), Some("src/graph"));
        // 4 distinct files; duplicate collapsed.
        assert_eq!(s.member_paths.len(), 4);
        assert!(s.text.contains("src/graph"));
        assert!(s.text.contains("4 files"));
    }

    #[test]
    fn summarize_handles_rootless_paths() {
        let members = vec!["README.md".to_string(), "LICENSE".to_string()];
        let s = summarize_cluster(&members);
        assert_eq!(s.member_count, 2);
        assert_eq!(s.top_dirs.first().map(|s| s.as_str()), Some("."));
    }
}
