//! Single-shot query correction.
//!
//! Thin wrapper over `wfst::query_rescore::rewrite_query` for callers
//! that want the corrected string + a confidence proxy without
//! caring about token-level mechanics. Used by `tool_correct_query`
//! alongside the existing `llammer_pipeline::lattice::LatticeCorrectionPipeline`
//! path (the two coexist — pgmcp picks whichever the project has
//! infrastructure for).
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use super::hybrid_lm::PgmcpHybridLm;
use super::query_rescore::{RewrittenQuery, rewrite_query};

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
}

/// Correct a query using the WFST pipeline. See [`rewrite_query`]
/// for parameter semantics.
pub fn correct_query_single(
    query: &str,
    max_distance: usize,
    edit_weight: f64,
    lm_weight: f64,
    vocabulary: &[String],
    lm: Option<&PgmcpHybridLm>,
) -> CorrectionResult {
    let RewrittenQuery {
        original,
        rewritten,
        changed,
        used_lm,
        ..
    } = rewrite_query(query, max_distance, edit_weight, lm_weight, vocabulary, lm);

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_unchanged_high_confidence() {
        let r = correct_query_single("", 2, 1.0, 0.0, &[], None);
        assert_eq!(r.input, "");
        assert_eq!(r.corrected, "");
        assert!(!r.changed);
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn no_correction_keeps_identity_high_confidence() {
        let r = correct_query_single("hello", 2, 1.0, 0.0, &[], None);
        assert_eq!(r.corrected, "hello");
        assert!(!r.changed);
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn corrected_no_lm_quarter_confidence() {
        let vocab = vec!["receive".to_string()];
        let r = correct_query_single("recieve", 2, -1.0, 0.0, &vocab, None);
        assert_eq!(r.corrected, "receive");
        assert!(r.changed);
        assert_eq!(r.confidence, 0.25);
    }
}
