//! Per-project HybridLanguageModel wrapper + loader/scorer.
//!
//! Wraps `libgrammstein::hybrid::HybridLanguageModel` (Modified
//! Kneser-Ney n-gram + subword embeddings + 4 interpolation
//! strategies) behind pgmcp's config surface AND adapts it to
//! `lling_llang::layers::rescoring::lm_rerank::LanguageModel` so the
//! WFST lattice in `src/wfst/lattice.rs` and the rescoring path in
//! `src/wfst/query_rescore.rs` can score candidate paths.
//!
//! Persistence uses libgrammstein's "portable" format
//! (`save_portable` / `load_portable`), which is backend-INDEPENDENT — the
//! on-disk `model.bin` stores `(term-id-byte key, NgramEntrySnapshot)` pairs +
//! the embedded vocabulary + embedding + config, never the store's in-memory
//! representation. pgmcp pins the backend to libgrammstein's byte-native
//! `TermIdStore<Arc<PersistentARTrie<NgramEntry>>>`: a `PersistentVocabARTrie`
//! maps words ↔ u64 term-ids and a byte `PersistentARTrie` holds the n-gram
//! counts keyed by raw LEB128 term-id byte sequences — self-delimiting, so
//! there is no `'|'` round-trip, no char lift (each varint byte stays a `u8`,
//! not a `u32` `char`), and no delimiter-collision hazard. On load the counts
//! trie is rebuilt as a fresh on-disk working file under `<model_dir>/live/`;
//! the vocabulary travels inside `model.bin` and is rebuilt by libgrammstein.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libdictenstein::persistent_artrie::PersistentARTrie;
use libgrammstein::hybrid::{HybridConfig, HybridLanguageModel, InterpolationStrategy};
use libgrammstein::ngram::{NgramEntry, SharedVocabARTrie, TermIdStore, open_or_create_vocabulary};
use lling_llang::layers::rescoring::lm_rerank::LanguageModel as LlingLanguageModel;
use thiserror::Error;

/// Byte counts backend for the per-project LM: a disk-backed byte
/// `PersistentARTrie` of `NgramEntry`, keyed on raw LEB128 term-id byte
/// sequences (self-delimiting — no `'|'`, no char lift).
pub(crate) type PgmcpLmBackend = Arc<PersistentARTrie<NgramEntry>>;

/// The per-project LM's n-gram store — libgrammstein's byte-native
/// [`TermIdStore`] over the disk counts backend. Replaces the char-lifted
/// `VocabularyIndexedDictionary<SharedCharARTrie<NgramEntry>>`: a
/// `PersistentVocabARTrie` maps words ↔ u64 term-ids and the byte counts trie
/// holds the term-id-keyed n-gram counts. The store is rebuilt from the
/// backend-independent portable `model.bin` on load (see module doc).
pub type PgmcpLmStore = TermIdStore<PgmcpLmBackend>;

/// Filenames of the two on-disk tries that back the LM, under a given working
/// dir (`<model_dir>/live` for readers, `<model_dir>/train` for the training
/// cron). Training uses both; loading uses only the counts trie (the vocabulary
/// travels inside `model.bin` and is rebuilt by libgrammstein).
pub(crate) fn lm_trie_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("vocab.artrie"), dir.join("counts.artrie"))
}

/// Build a FRESH byte counts backend (disk ART) for the LM's n-gram counts.
/// The caller must ensure the path's dir is wiped first — `create` reuses, not
/// truncates, an existing file.
pub(crate) fn try_build_counts_backend(counts_path: &Path) -> Result<PgmcpLmBackend, LmError> {
    Ok(Arc::new(
        PersistentARTrie::<NgramEntry>::create(counts_path)
            .map_err(|e| LmError::Grammstein(format!("counts trie: {e}")))?,
    ))
}

/// Infallible wrapper for the `FnOnce() -> B` backend factory that
/// [`HybridLanguageModel::load_portable`] requires (it has no error channel).
/// A persistent-trie I/O failure here means the model working dir is
/// unrecoverable, so panicking with a clear message is the right behavior.
pub(crate) fn build_counts_backend(counts_path: &Path) -> PgmcpLmBackend {
    try_build_counts_backend(counts_path).expect("hybrid-lm: build persistent counts backend")
}

/// Build FRESH training inputs for the per-project LM: the byte counts backend
/// (disk ART for the n-gram counts) plus a persistent word ↔ term-id vocabulary.
/// The trainer (`TrainerBuilder::new(backend).with_vocabulary(vocab)`) wires the
/// two into a [`TermIdStore`] internally. Shared by the training cron and its
/// round-trip test. The caller must ensure the dir is wiped first — `create` and
/// `open_or_create_vocabulary` reuse, not truncate, existing files.
pub(crate) fn try_build_lm_training_inputs(
    vocab_path: &Path,
    counts_path: &Path,
) -> Result<(PgmcpLmBackend, SharedVocabARTrie), LmError> {
    let vocab = open_or_create_vocabulary(vocab_path)
        .map_err(|e| LmError::Grammstein(format!("vocab trie: {e}")))?;
    Ok((try_build_counts_backend(counts_path)?, vocab))
}

/// pgmcp-side config knob for the n-gram-LM third leg of hybrid_search.
/// Maps directly to libgrammstein's `HybridConfig`; carried separately
/// so the cron and the tool can read it without taking a libgrammstein
/// dep through the config crate.
#[derive(Debug, Clone)]
pub struct HybridLmConfig {
    /// N-gram order (1-5). Default 3 (trigram).
    pub order: usize,
    /// Interpolation strategy between n-gram and subword embedding
    /// scores. Default `Linear { alpha: 0.8 }` — n-gram dominates.
    pub strategy: InterpolationStrategy,
    /// Score-cache size in entries. Default 50,000.
    pub cache_size: usize,
    /// Embedding smoothing constant. Default 1e-8.
    pub embedding_smoothing: f64,
    /// Softmax temperature. Default 1.0.
    pub temperature: f64,
}

impl Default for HybridLmConfig {
    fn default() -> Self {
        Self {
            order: 3,
            strategy: InterpolationStrategy::Linear { alpha: 0.8 },
            cache_size: 50_000,
            embedding_smoothing: 1e-8,
            temperature: 1.0,
        }
    }
}

impl HybridLmConfig {
    /// Convert into a libgrammstein `HybridConfig` for handing to
    /// `HybridLanguageModel::new`.
    pub fn to_grammstein(&self) -> HybridConfig {
        HybridConfig {
            strategy: self.strategy,
            cache_size: self.cache_size,
            embedding_smoothing: self.embedding_smoothing,
            temperature: self.temperature,
        }
    }
}

/// Errors from the LM loader.
#[derive(Debug, Error)]
pub enum LmError {
    /// I/O failure during open/save.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// libgrammstein returned an error during portable load/save.
    #[error("libgrammstein: {0}")]
    Grammstein(String),
}

impl From<libgrammstein::Error> for LmError {
    fn from(value: libgrammstein::Error) -> Self {
        LmError::Grammstein(format!("{value:?}"))
    }
}

/// Loaded per-project hybrid LM. `Arc`-wrapping the inner model lets
/// multiple lattice/rescorer entry points share the cache without
/// per-call clones.
#[derive(Clone)]
pub struct PgmcpHybridLm {
    inner: Arc<HybridLanguageModel<PgmcpLmStore>>,
    path: PathBuf,
}

impl PgmcpHybridLm {
    /// Load a previously-trained model from the portable bincode
    /// format produced by `cron::ngram_lm_train`.
    ///
    /// The counts trie is rebuilt from `model.bin` into a fresh on-disk working
    /// file under `<model_dir>/live/` (the vocabulary is embedded in `model.bin`
    /// and rebuilt by libgrammstein). The `live/` dir is wiped first —
    /// `DiskManager::create` reuses, not truncates, an existing file, and stale
    /// WAL/archive segments could corrupt a "fresh" trie.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LmError> {
        let path = path.as_ref().to_path_buf();
        let model_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let live_dir = model_dir.join("live");
        let _ = std::fs::remove_dir_all(&live_dir);
        std::fs::create_dir_all(&live_dir)?;
        let (_, counts_path) = lm_trie_paths(&live_dir);

        // `load_portable`'s factory is `FnOnce() -> B` (no error channel), so
        // the infallible builder is used here; trie I/O failure surfaces as a
        // panic with a clear message (a corrupt model dir is unrecoverable).
        // Only the counts backend is supplied — the vocabulary is embedded in
        // `model.bin` and rebuilt by libgrammstein on load.
        let factory = move || build_counts_backend(&counts_path);
        let inner = HybridLanguageModel::<PgmcpLmStore>::load_portable(&path, factory)?;
        Ok(Self {
            inner: Arc::new(inner),
            path,
        })
    }

    /// Wrap an already-constructed model (used by tests and the
    /// training cron's in-process round-trip after save).
    pub fn from_loaded(inner: HybridLanguageModel<PgmcpLmStore>, path: PathBuf) -> Self {
        Self {
            inner: Arc::new(inner),
            path,
        }
    }

    /// Score a continuation (single word given prefix context).
    pub fn score_continuation(&self, prefix: &[&str], next: &str) -> f64 {
        self.inner.score(next, prefix)
    }

    /// Path the model was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reference to the underlying libgrammstein model (escape hatch
    /// for tests + the integration::lazy_ngram WFST state-source
    /// callers if they need direct access).
    pub fn inner(&self) -> &HybridLanguageModel<PgmcpLmStore> {
        &self.inner
    }
}

/// Adapter for lling-llang's `LanguageModel` trait. The trait wants
/// `score_sequence(&[&str])` (returns total log-prob of the sequence)
/// and `score_continuation(&[&str], &str)` (returns log-prob of one
/// word given prefix); we delegate both to `HybridLanguageModel::score`.
impl LlingLanguageModel for PgmcpHybridLm {
    fn score_sequence(&self, tokens: &[&str]) -> f64 {
        if tokens.is_empty() {
            return 0.0;
        }
        let mut total = 0.0;
        for (i, tok) in tokens.iter().enumerate() {
            let prefix: Vec<&str> = tokens[..i].to_vec();
            total += self.inner.score(tok, &prefix);
        }
        total
    }

    fn score_continuation(&self, prefix: &[&str], next: &str) -> f64 {
        self.inner.score(next, prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_documented_values() {
        let cfg = HybridLmConfig::default();
        assert_eq!(cfg.order, 3);
        assert_eq!(cfg.cache_size, 50_000);
        assert_eq!(cfg.embedding_smoothing, 1e-8);
        assert_eq!(cfg.temperature, 1.0);
        match cfg.strategy {
            InterpolationStrategy::Linear { alpha } => assert_eq!(alpha, 0.8),
            _ => panic!("default strategy should be Linear {{ alpha: 0.8 }}"),
        }
    }

    #[test]
    fn to_grammstein_preserves_fields() {
        let cfg = HybridLmConfig::default();
        let g = cfg.to_grammstein();
        assert_eq!(g.cache_size, cfg.cache_size);
        assert_eq!(g.embedding_smoothing, cfg.embedding_smoothing);
        assert_eq!(g.temperature, cfg.temperature);
    }
}
