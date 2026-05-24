//! Per-project n-gram + subword-embedding hybrid LM training cron.
//!
//! Streams `file_chunks.content` rows per project, trains a
//! libgrammstein `HybridLanguageModel` (Modified Kneser-Ney n-gram +
//! subword embeddings), persists to disk in the portable bincode
//! format consumed by `wfst::hybrid_lm::PgmcpHybridLm::open`.
//!
//! Resume-on-restart: the cron writes a `<model_path>.tmp` first and
//! atomically renames on success, so a partial write never produces a
//! corrupted model. Re-training is idempotent — the next run rebuilds
//! the model from scratch (n-gram training is not incremental in the
//! current libgrammstein API).
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use libgrammstein::corpus::{CorpusReader, Document};
use libgrammstein::embedding::EmbeddingTrainerBuilder;
use libgrammstein::hybrid::HybridLanguageModel;
use libgrammstein::ngram::{NgramEntry, TrainerBuilder};
use liblevenshtein::dictionary::pathmap::PathMapDictionary;
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::stats::tracker::StatsTracker;
use crate::wfst::hybrid_lm::{HybridLmConfig, PgmcpLmDictionary};

/// Cron entry point. Runs across all projects; logs and continues
/// on per-project errors.
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>, data_dir: PathBuf) {
    stats.ngram_lm_train_runs.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = run_pass(&pool, &stats, &data_dir).await {
        warn!(error = %e, "ngram-lm-train pass failed");
    }
}

/// Run one training pass for every project.
pub async fn run_pass(
    pool: &PgPool,
    stats: &StatsTracker,
    data_dir: &Path,
) -> Result<(), sqlx::Error> {
    let projects: Vec<(i32, String)> =
        sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await?;
    for (project_id, project_name) in projects {
        match train_project(pool, project_id, &project_name, data_dir).await {
            Ok(true) => {
                stats
                    .ngram_lm_train_projects_trained
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {
                debug!(
                    project = %project_name,
                    "ngram-lm-train: insufficient chunks; skipping"
                );
            }
            Err(e) => {
                warn!(project = %project_name, error = %e, "ngram-lm-train: per-project failure");
            }
        }
    }
    Ok(())
}

/// Train one project's HybridLanguageModel. Returns `Ok(true)` if a
/// new model was written, `Ok(false)` if the project has too few
/// chunks to train.
async fn train_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    data_dir: &Path,
) -> Result<bool, TrainError> {
    let contents: Vec<String> = sqlx::query_scalar::<_, String>(
        "SELECT fc.content
         FROM file_chunks fc
         JOIN indexed_files f ON fc.file_id = f.id
         WHERE f.project_id = $1
           AND fc.content IS NOT NULL
           AND length(fc.content) > 0",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    if contents.len() < 5 {
        return Ok(false);
    }

    // Subword-embedding training needs ≥ ~10 distinct tokens; if the
    // corpus is too small the embedding trainer panics. Skip rather
    // than crash on tiny projects.
    let total_tokens: usize = contents.iter().map(|c| c.split_whitespace().count()).sum();
    if total_tokens < 50 {
        return Ok(false);
    }

    let cfg = HybridLmConfig::default();
    let order = cfg.order;

    // Train the n-gram side.
    let reader_ngram = ChunkCorpus::new(contents.clone());
    let dictionary = PathMapDictionary::<NgramEntry>::new();
    let ngram_model = TrainerBuilder::new(dictionary)
        .order(order)
        .train(reader_ngram)
        .map_err(|e| TrainError::Train(format!("ngram: {e}")))?;

    // Train the subword embedding side. Tiny dims + few epochs keep
    // per-project cron time bounded (typically <30s for a 10k-chunk
    // project).
    let reader_emb = ChunkCorpus::new(contents);
    let embedding = EmbeddingTrainerBuilder::new()
        .dim(64)
        .window_size(3)
        .min_count(2)
        .epochs(2)
        .train(reader_emb)
        .map_err(|e| TrainError::Train(format!("embedding: {e}")))?;

    let model: HybridLanguageModel<PgmcpLmDictionary> =
        HybridLanguageModel::new(ngram_model, embedding, cfg.to_grammstein());

    let model_path = model_path_for(data_dir, project_name);
    if let Some(parent) = model_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = model_path.with_extension("bin.tmp");
    model
        .save_portable(&tmp_path)
        .map_err(|e| TrainError::Train(format!("save: {e}")))?;
    std::fs::rename(&tmp_path, &model_path)?;

    info!(
        project = %project_name,
        path = %model_path.display(),
        n_chunks = total_tokens,
        "ngram-lm-train persisted hybrid LM"
    );
    Ok(true)
}

/// Canonical on-disk location for a project's HybridLM model.
/// Shape: `<data_dir>/hybrid_lm/<slug>/model.bin`. The slug is the
/// project's name (already constrained to a safe identifier by the
/// scanner).
pub fn model_path_for(data_dir: &Path, project_name: &str) -> PathBuf {
    data_dir
        .join("hybrid_lm")
        .join(project_name)
        .join("model.bin")
}

/// `CorpusReader` implementation over an in-memory `Vec<String>`.
/// libgrammstein's `Tokenizer::sentences` lowercases and strips
/// punctuation; here we just hand it the file-chunk contents
/// verbatim and let the upstream tokenizer do its job.
struct ChunkCorpus {
    chunks: Vec<String>,
}

impl ChunkCorpus {
    fn new(chunks: Vec<String>) -> Self {
        Self { chunks }
    }
}

impl CorpusReader for ChunkCorpus {
    fn documents(&self) -> Box<dyn Iterator<Item = Document> + Send + '_> {
        Box::new(self.chunks.iter().cloned().map(Document::new))
    }

    fn sentences(&self) -> Box<dyn Iterator<Item = String> + Send + '_> {
        // Split each chunk into sentence-ish lines so the n-gram
        // trainer sees one sentence per line. libgrammstein's
        // tokenizer expects one sentence per item.
        Box::new(self.chunks.iter().flat_map(|c| {
            c.split('\n')
                .map(|l| l.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        }))
    }

    fn document_count(&self) -> Option<usize> {
        Some(self.chunks.len())
    }
}

#[derive(Debug, thiserror::Error)]
enum TrainError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("train: {0}")]
    Train(String),
}
