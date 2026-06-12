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
//! on-disk `model.bin` stores `(key, NgramEntrySnapshot)` pairs + embedding +
//! config, never the dictionary's in-memory representation. pgmcp pins the
//! backend to a vocabulary-indexed disk-backed ART trie
//! (`VocabularyIndexedDictionary<SharedCharARTrie<NgramEntry>>`): a
//! `PersistentVocabARTrie` maps words ↔ u64 ids and a `PersistentARTrieChar`
//! holds the integer-keyed n-gram counts. The vocab + counts tries are real
//! on-disk working files rebuilt from `model.bin` on load.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libdictenstein::persistent_artrie::char::{PersistentARTrieChar, SharedCharARTrie};
use libgrammstein::hybrid::{HybridConfig, HybridLanguageModel, InterpolationStrategy};
use libgrammstein::ngram::NgramEntry;
use libgrammstein::ngram::open_or_create_vocabulary;
use libgrammstein::ngram::vocabulary_indexed::VocabularyIndexedDictionary;
use lling_llang::layers::rescoring::lm_rerank::LanguageModel as LlingLanguageModel;
use thiserror::Error;

/// Concrete dictionary backend pgmcp uses for the per-project LM: a
/// vocabulary-indexed dictionary over a disk-backed char ARTrie. The
/// vocabulary (`SharedVocabARTrie`, a `PersistentVocabARTrie`) maps words ↔
/// u64 ids; the n-gram counts live in a `PersistentARTrieChar`-backed
/// `SharedCharARTrie`. The delimiter is pinned to `'|'` to match
/// libgrammstein's `LEGACY_NGRAM_SEPARATOR`, so portable keys round-trip
/// through `NgramTrie`'s legacy split/join. The runtime dictionary is rebuilt
/// from the backend-independent portable `model.bin` on load (see module doc).
pub type PgmcpLmDictionary = VocabularyIndexedDictionary<SharedCharARTrie<NgramEntry>>;

/// Delimiter pinned to libgrammstein's `LEGACY_NGRAM_SEPARATOR` (`'|'`) — the
/// `VocabularyIndexedDictionary` MUST split/join on the same char `NgramTrie`
/// uses for its legacy keys, or the portable round-trip silently breaks.
const LM_NGRAM_DELIMITER: char = '|';

/// Filenames of the two on-disk tries that back the LM dictionary, under a
/// given working dir (`<model_dir>/live` for readers, `<model_dir>/train` for
/// the training cron).
pub(crate) fn lm_trie_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("vocab.artrie"), dir.join("counts.artrie"))
}

/// Build an empty vocabulary-indexed LM dictionary backed by FRESH on-disk
/// tries (vocab ↔ id + integer-keyed n-gram counts) with the pinned `'|'`
/// delimiter. Shared by the loader ([`PgmcpHybridLm::open`]) and the training
/// cron. The caller must ensure the paths are empty (their dir wiped) first —
/// `PersistentARTrieChar::create` reuses, not truncates, an existing file.
pub(crate) fn try_build_lm_dictionary(
    vocab_path: &Path,
    counts_path: &Path,
) -> Result<PgmcpLmDictionary, LmError> {
    let vocab = open_or_create_vocabulary(vocab_path)
        .map_err(|e| LmError::Grammstein(format!("vocab trie: {e}")))?;
    // `SharedCharARTrie<V> = Arc<PersistentARTrieChar<V>>` since libdictenstein's
    // overlay refactor moved concurrency inside the trie (no external `RwLock`).
    let counts: SharedCharARTrie<NgramEntry> = Arc::new(
        PersistentARTrieChar::<NgramEntry>::create(counts_path)
            .map_err(|e| LmError::Grammstein(format!("counts trie: {e}")))?,
    );
    Ok(VocabularyIndexedDictionary::with_delimiter(
        counts,
        vocab,
        LM_NGRAM_DELIMITER,
    ))
}

/// Infallible wrapper for the `FnOnce() -> D` factory that
/// [`HybridLanguageModel::load_portable`] requires (it has no error channel).
/// A persistent-trie I/O failure here means the model working dir is
/// unrecoverable, so panicking with a clear message is the right behavior.
pub(crate) fn build_lm_dictionary(vocab_path: &Path, counts_path: &Path) -> PgmcpLmDictionary {
    try_build_lm_dictionary(vocab_path, counts_path)
        .expect("hybrid-lm: build persistent LM dictionary")
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
    inner: Arc<HybridLanguageModel<PgmcpLmDictionary>>,
    path: PathBuf,
}

impl PgmcpHybridLm {
    /// Load a previously-trained model from the portable bincode
    /// format produced by `cron::ngram_lm_train`.
    ///
    /// The runtime dictionary is rebuilt from `model.bin` into fresh on-disk
    /// vocab + counts tries under `<model_dir>/live/`. These are working
    /// storage derived from the (crash-safe) portable file, so the dir is
    /// wiped first — `DiskManager::create` reuses, not truncates, an existing
    /// file, and stale WAL/archive segments could corrupt a "fresh" trie.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LmError> {
        let path = path.as_ref().to_path_buf();
        let model_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let live_dir = model_dir.join("live");
        let _ = std::fs::remove_dir_all(&live_dir);
        std::fs::create_dir_all(&live_dir)?;
        let vocab_path = live_dir.join("vocab.artrie");
        let counts_path = live_dir.join("counts.artrie");

        // `load_portable`'s factory is `FnOnce() -> D` (no error channel), so
        // the infallible builder is used here; trie I/O failure surfaces as a
        // panic with a clear message (a corrupt model dir is unrecoverable).
        let factory = move || build_lm_dictionary(&vocab_path, &counts_path);
        let inner = HybridLanguageModel::<PgmcpLmDictionary>::load_portable(&path, factory)?;
        Ok(Self {
            inner: Arc::new(inner),
            path,
        })
    }

    /// Wrap an already-constructed model (used by tests and the
    /// training cron's in-process round-trip after save).
    pub fn from_loaded(inner: HybridLanguageModel<PgmcpLmDictionary>, path: PathBuf) -> Self {
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
    pub fn inner(&self) -> &HybridLanguageModel<PgmcpLmDictionary> {
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
