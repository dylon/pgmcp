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
use libgrammstein::ngram::TrainerBuilder;
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::stats::tracker::StatsTracker;
use crate::wfst::hybrid_lm::{
    HybridLmConfig, PgmcpLmDictionary, lm_trie_paths, try_build_lm_dictionary,
};

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

    // Build the vocab-indexed persistent dictionary backed by fresh on-disk
    // tries under the project's model dir (a `train/` subdir, kept separate
    // from readers' `live/` dir so the cron and a reader never share a trie
    // path). Wipe it first so the tries start empty (`create` reuses, not
    // truncates, existing files).
    let model_path = model_path_for(data_dir, project_name);
    let train_dir = model_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.to_path_buf())
        .join("train");
    let _ = std::fs::remove_dir_all(&train_dir);
    std::fs::create_dir_all(&train_dir)?;
    let (vocab_path, counts_path) = lm_trie_paths(&train_dir);
    let dictionary = try_build_lm_dictionary(&vocab_path, &counts_path)
        .map_err(|e| TrainError::Train(format!("lm dictionary: {e}")))?;

    // Train the n-gram side.
    let reader_ngram = ChunkCorpus::new(contents.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wfst::hybrid_lm::PgmcpHybridLm;
    use libgrammstein::embedding::EmbeddingTrainerBuilder;

    /// End-to-end proof of the vocab-indexed persistent LM backend: train →
    /// `save_portable` → `open` (which rebuilds the on-disk vocab+counts tries
    /// from `model.bin`) → score, plus stability across a reload. Exercises
    /// both `IterableDictionary` impls (save iterates the wrapper → decodes to
    /// `'|'`-joined keys; load replays them) and the `'|'` delimiter invariant
    /// (a mismatch would corrupt the round-trip silently).
    #[test]
    fn vocab_indexed_lm_roundtrip_train_save_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path();
        let project = "roundtrip_proj";

        // ≥5 chunks, ≥50 tokens, and enough distinct tokens to satisfy the
        // embedding trainer's minimum-vocabulary requirement.
        let contents: Vec<String> = (0..12)
            .map(|i| {
                format!(
                    "the quick brown fox jumps over the lazy dog number {i} \
                     and then the quick brown cat runs away very fast today"
                )
            })
            .collect();

        let cfg = HybridLmConfig::default();
        let model_path = model_path_for(data_dir, project);
        let train_dir = model_path
            .parent()
            .expect("model path has parent")
            .join("train");
        std::fs::create_dir_all(&train_dir).expect("mkdir train");
        let (vocab_path, counts_path) = lm_trie_paths(&train_dir);
        let dictionary =
            try_build_lm_dictionary(&vocab_path, &counts_path).expect("build dictionary");

        let ngram_model = TrainerBuilder::new(dictionary)
            .order(cfg.order)
            .train(ChunkCorpus::new(contents.clone()))
            .expect("train ngram");
        let embedding = EmbeddingTrainerBuilder::new()
            .dim(16)
            .window_size(3)
            .min_count(1)
            .epochs(1)
            .train(ChunkCorpus::new(contents))
            .expect("train embedding");
        let model: HybridLanguageModel<PgmcpLmDictionary> =
            HybridLanguageModel::new(ngram_model, embedding, cfg.to_grammstein());
        std::fs::create_dir_all(model_path.parent().expect("parent")).expect("mkdir model dir");
        model.save_portable(&model_path).expect("save_portable");

        // The vocab-indexed backend really did write both persistent tries.
        assert!(vocab_path.exists(), "vocab.artrie was created on disk");
        assert!(counts_path.exists(), "counts.artrie was created on disk");

        // The production loader rebuilds the tries under `live/` and scores.
        let lm = PgmcpHybridLm::open(&model_path).expect("open");
        let s1 = lm.score_continuation(&["quick", "brown"], "fox");
        assert!(s1.is_finite(), "score is finite: {s1}");

        // Reload → identical score: the portable round-trip is deterministic.
        let s2 = PgmcpHybridLm::open(&model_path)
            .expect("reopen")
            .score_continuation(&["quick", "brown"], "fox");
        assert!(
            (s1 - s2).abs() < 1e-9,
            "score stable across reload: {s1} vs {s2}"
        );
    }
}
