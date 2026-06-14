//! Topic-dendrogram cron — hierarchical-agglomerative clustering +
//! c-TF-IDF keyword extraction, persisted to the
//! `topic_dendrograms` table.
//!
//! Sits beside the existing online FCM (`topic_clustering_online`)
//! which owns the per-chunk soft assignments. This cron consumes the
//! same chunks but produces a hierarchy with crash-resume
//! checkpointing — the user-facing `dendrogram_topic_hierarchy`
//! MCP tool (Phase 8) reads from it.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 7.

use std::sync::Arc;

use bincode::Options;
use libgrammstein::topic::{TopicConfig, TopicExtractor};
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::stats::tracker::StatsTracker;

/// Upper bound on chunks fed to the O(n²) agglomerative extractor. ~6k chunks
/// → ~280 MB distance matrix; larger projects are strided-subsampled. This is
/// the fix for the empty-`topic_dendrograms` bug (giant projects OOMed the
/// extractor and the error was swallowed).
const MAX_DENDROGRAM_CHUNKS: usize = 6_000;

/// Per-run outcome.
#[derive(Debug, Default, Clone)]
pub struct DendrogramRunReport {
    pub projects_processed: u64,
    pub topics_generated: u64,
    pub errors: u64,
}

/// Daemon-facing entry point. Iterates active projects, runs the
/// hierarchical-agglomerative + c-TF-IDF pipeline for each, persists
/// the result. Skips projects with <2 chunks (the extractor refuses
/// at n < 2 anyway).
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>) {
    if let Err(e) = run_pass(&pool, &stats).await {
        warn!(error = %e, "topic-dendrogram pass failed");
    }
}

/// Run a single dendrogram pass across all projects.
pub async fn run_pass(
    pool: &PgPool,
    stats: &StatsTracker,
) -> Result<DendrogramRunReport, sqlx::Error> {
    stats
        .topic_dendrogram_runs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let projects: Vec<(i32, String)> =
        sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await?;

    let mut report = DendrogramRunReport::default();
    for (project_id, project_name) in projects {
        match run_project(pool, project_id, &project_name).await {
            Ok(topic_count) => {
                report.projects_processed += 1;
                report.topics_generated += topic_count;
                stats
                    .topic_dendrogram_topics_generated
                    .fetch_add(topic_count, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                warn!(project = %project_name, error = %e, "topic-dendrogram project run failed");
                report.errors += 1;
            }
        }
    }
    Ok(report)
}

async fn run_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
) -> Result<u64, sqlx::Error> {
    // Read chunks for this project via the signature-aware bulk extract
    // helper (lands in C8 as queries::bulk_extract_project_embeddings).
    let chunks =
        crate::db::queries::bulk_extract_project_embeddings(pool, project_name, None).await?;
    if chunks.len() < 2 {
        debug!(
            project = project_name,
            n = chunks.len(),
            "topic-dendrogram: insufficient chunks; skipping"
        );
        return Ok(0);
    }

    // Bound the input before the agglomerative extractor. libgrammstein's
    // `TopicExtractor::extract` builds an O(n²) distance matrix, so a giant
    // project (e.g. `claude` at ~167k chunks → ~114 GB) OOMs and the error is
    // swallowed by `run_pass`, leaving `topic_dendrograms` permanently empty.
    // A deterministic strided subsample caps n at `MAX_DENDROGRAM_CHUNKS`
    // (~6k → ~280 MB matrix), which makes the dendrogram populate for ALL
    // projects, including the giants, while spanning the whole corpus.
    let (embeddings, documents) = if chunks.len() > MAX_DENDROGRAM_CHUNKS {
        let stride = chunks.len() / MAX_DENDROGRAM_CHUNKS;
        let stride = stride.max(1);
        let sampled: Vec<&crate::db::queries::ChunkEmbeddingRow> = chunks
            .iter()
            .step_by(stride)
            .take(MAX_DENDROGRAM_CHUNKS)
            .collect();
        info!(
            project = project_name,
            total = chunks.len(),
            sampled = sampled.len(),
            "topic-dendrogram: subsampling large project before agglomerative clustering"
        );
        (
            sampled
                .iter()
                .map(|c| c.embedding.clone())
                .collect::<Vec<_>>(),
            sampled
                .iter()
                .map(|c| c.content.clone())
                .collect::<Vec<_>>(),
        )
    } else {
        (
            chunks
                .iter()
                .map(|c| c.embedding.clone())
                .collect::<Vec<_>>(),
            chunks.iter().map(|c| c.content.clone()).collect::<Vec<_>>(),
        )
    };
    let mut extractor = TopicExtractor::new(TopicConfig::default());
    let result = match extractor.extract(&embeddings, &documents) {
        Ok(r) => r,
        Err(e) => {
            return Err(sqlx::Error::Configuration(
                format!("topic-dendrogram extractor: {e}").into(),
            ));
        }
    };
    let n_topics = result.topics.len() as u64;

    // Persist. The dendrogram is serialized to a bincode blob so
    // downstream consumers (the MCP tool in Phase 8) can re-load it
    // without re-running the heavy clustering.
    let blob = bincode::DefaultOptions::new()
        .serialize(&result.topics)
        .map_err(|e| sqlx::Error::Configuration(format!("serialize topics: {e}").into()))?;
    // c-TF-IDF keywords flattened to JSONB-friendly Vec<Vec<String>>.
    let keywords: Vec<Vec<String>> = result
        .topics
        .iter()
        .map(|t| {
            t.keywords
                .iter()
                .map(|(term, _score)| term.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    let keywords_json = serde_json::to_value(&keywords)
        .map_err(|e| sqlx::Error::Configuration(format!("serialize keywords: {e}").into()))?;
    sqlx::query(
        "INSERT INTO topic_dendrograms (project_id, dendrogram_blob, ctfidf_keywords, generated_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (project_id) DO UPDATE SET
            dendrogram_blob = EXCLUDED.dendrogram_blob,
            ctfidf_keywords = EXCLUDED.ctfidf_keywords,
            generated_at = EXCLUDED.generated_at",
    )
    .bind(project_id)
    .bind(&blob)
    .bind(&keywords_json)
    .execute(pool)
    .await?;
    info!(
        project = project_name,
        topics = n_topics,
        "topic-dendrogram persisted"
    );
    Ok(n_topics)
}
