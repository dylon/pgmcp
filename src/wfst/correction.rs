//! Single-shot query correction.
//!
//! Thin wrapper over `wfst::query_rescore::rewrite_query` for callers
//! that want the corrected string + a confidence proxy without
//! caring about token-level mechanics. P14.5 — signature now takes
//! `&FuzzyIndex<V>` (the persistent symbol trie), mirroring
//! `rewrite_query`.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2 + Phase 14.5.

use libdictenstein::DictionaryValue;

use super::hybrid_lm::PgmcpHybridLm;
use super::query_rescore::{RewrittenQuery, rewrite_query};
use crate::fuzzy::persistent_artrie::FuzzyIndex;

/// Single-shot correction result.
#[derive(Debug, Clone)]
pub struct CorrectionResult {
    /// Original query (verbatim).
    pub input: String,
    /// Corrected query (whitespace-joined Viterbi best).
    pub corrected: String,
    /// True iff `corrected != input.to_lowercase()`.
    pub changed: bool,
    /// Confidence proxy in `[0, 1]`. Tiering: `1.0` when the path is
    /// unchanged (identity wins); `0.5` when LM rescoring was applied;
    /// `0.25` when only edit-distance scoring was available.
    pub confidence: f64,
    /// Whether the per-project n-gram language model was applied during
    /// lattice rescoring (false when no model exists or `lm_weight == 0`).
    pub used_lm: bool,
}

/// Correct a query using the WFST pipeline. See [`rewrite_query`]
/// for parameter semantics.
#[allow(clippy::too_many_arguments)]
pub fn correct_query_single<V>(
    query: &str,
    max_distance: usize,
    edit_weight: f64,
    lm_weight: f64,
    phonetic_cost_weight: f64,
    phonetic_max_total_cost: f64,
    fuzzy_idx: &FuzzyIndex<V>,
    lm: Option<&PgmcpHybridLm>,
) -> CorrectionResult
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    let RewrittenQuery {
        original,
        rewritten,
        changed,
        used_lm,
        ..
    } = rewrite_query(
        query,
        max_distance,
        edit_weight,
        lm_weight,
        phonetic_cost_weight,
        phonetic_max_total_cost,
        fuzzy_idx,
        lm,
    );

    let confidence = if !changed {
        1.0
    } else if used_lm {
        0.5
    } else {
        0.25
    };

    CorrectionResult {
        input: original,
        corrected: rewritten,
        changed,
        confidence,
        used_lm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only helper: build an empty in-memory `FuzzyIndex<()>`
    /// in a tempdir.
    fn empty_trie() -> (tempfile::TempDir, FuzzyIndex<()>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.artrie");
        let (idx, _recovery) = FuzzyIndex::<()>::open_or_create(&path).expect("create");
        (tmp, idx)
    }

    fn trie_with(terms: &[&str]) -> (tempfile::TempDir, FuzzyIndex<()>) {
        let (tmp, idx) = empty_trie();
        for term in terms {
            idx.upsert(term, ()).expect("upsert");
        }
        (tmp, idx)
    }

    #[test]
    fn empty_query_unchanged_high_confidence() {
        let (_tmp, idx) = empty_trie();
        let r = correct_query_single("", 2, 1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(r.input, "");
        assert_eq!(r.corrected, "");
        assert!(!r.changed);
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn no_correction_keeps_identity_high_confidence() {
        let (_tmp, idx) = empty_trie();
        let r = correct_query_single("hello", 2, 1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(r.corrected, "hello");
        assert!(!r.changed);
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn corrected_no_lm_quarter_confidence() {
        let (_tmp, idx) = trie_with(&["receive"]);
        let r = correct_query_single("recieve", 2, -1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(r.corrected, "receive");
        assert!(r.changed);
        assert_eq!(r.confidence, 0.25);
    }
}
