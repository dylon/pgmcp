//! Query rewriting via WFST lattice + HybridLM rescoring.
//!
//! Entry point used by both `tool_hybrid_search` (third RRF leg) and
//! `tool_correct_query` (single-shot user-facing correction). The
//! function takes a free-form query, tokenizes it, generates
//! Damerau-Levenshtein candidates against a per-token candidate
//! function, builds a TropicalWeight lattice, optionally rescores
//! with the per-project HybridLM, and returns the Viterbi-best
//! rewritten query.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;

use super::hybrid_lm::PgmcpHybridLm;
use super::lattice::{TokenCandidate, build_correction_lattice, rescore_with_lm, viterbi_best};

/// Result of a single rewrite call.
#[derive(Debug, Clone)]
pub struct RewrittenQuery {
    /// The original query string (verbatim).
    pub original: String,
    /// The Viterbi-best rewrite (joined tokens). When no candidates
    /// improve the score, this equals `original`.
    pub rewritten: String,
    /// Whether `rewritten` differs from `original` (case-sensitive).
    pub changed: bool,
    /// Number of input tokens (post-tokenization).
    pub token_count: usize,
    /// Whether the language model was applied to the lattice.
    pub used_lm: bool,
}

/// Tokenizer compatible with both the WFST candidate generator and
/// `tool_hybrid_search`'s downstream search calls. Splits on Unicode
/// whitespace, drops empty spans, lower-cases for matching against
/// the (lower-cased) symbol dictionary.
pub fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Rewrite a query through the WFST pipeline.
///
/// `vocabulary` is the per-project symbol vocabulary the lattice will
/// search against for candidates. The Transducer is built once per
/// call from the full vocabulary; for large vocabularies the caller
/// can pre-build it (see [`rewrite_query_with_transducer`]).
///
/// `lm_weight` interpolates LM cost into the lattice edge weights
/// (0.0 = ignore LM, 1.0 = LM only). When `lm` is `None` the LM step
/// is skipped entirely — the returned `RewrittenQuery.used_lm` field
/// signals that.
pub fn rewrite_query(
    query: &str,
    max_distance: usize,
    edit_weight: f64,
    lm_weight: f64,
    vocabulary: &[String],
    lm: Option<&PgmcpHybridLm>,
) -> RewrittenQuery {
    let tokens = tokenize_query(query);
    if tokens.is_empty() {
        return RewrittenQuery {
            original: query.to_string(),
            rewritten: query.to_string(),
            changed: false,
            token_count: 0,
            used_lm: false,
        };
    }

    // Build a Damerau-Levenshtein Transducer over the vocabulary once.
    // Empty vocabulary → no candidates, identity path wins trivially.
    let candidates_per_token: Vec<Vec<TokenCandidate>> = if vocabulary.is_empty() {
        tokens.iter().map(|_| Vec::new()).collect()
    } else {
        let dict: DynamicDawgChar<()> =
            DynamicDawgChar::from_terms(vocabulary.iter().map(|s| s.as_str()));
        let xducer = Transducer::with_transposition(dict);
        tokens
            .iter()
            .map(|tok| {
                xducer
                    .query_with_distance(tok, max_distance)
                    .map(|c| TokenCandidate {
                        term: c.term,
                        distance: c.distance,
                    })
                    .collect()
            })
            .collect()
    };

    let token_refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let base_lattice = build_correction_lattice(&token_refs, &candidates_per_token, edit_weight);

    let (lattice, used_lm) = match (lm, lm_weight > 0.0) {
        (Some(lm), true) => match rescore_with_lm(&base_lattice, lm, lm_weight) {
            Ok(l) => (l, true),
            Err(_) => (base_lattice, false),
        },
        _ => (base_lattice, false),
    };

    let out = match viterbi_best(&lattice) {
        Ok(out) => out,
        Err(_) => {
            return RewrittenQuery {
                original: query.to_string(),
                rewritten: query.to_string(),
                changed: false,
                token_count: tokens.len(),
                used_lm,
            };
        }
    };

    let rewritten = out.viterbi_path.join(" ");
    let changed = rewritten != tokens.join(" ");
    RewrittenQuery {
        original: query.to_string(),
        rewritten,
        changed,
        token_count: tokens.len(),
        used_lm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_unchanged() {
        let out = rewrite_query("", 2, 1.0, 0.0, &[], None);
        assert_eq!(out.original, "");
        assert_eq!(out.rewritten, "");
        assert_eq!(out.token_count, 0);
        assert!(!out.changed);
    }

    #[test]
    fn empty_vocabulary_identity_pass_through() {
        let out = rewrite_query("hello world", 2, 1.0, 0.0, &[], None);
        assert_eq!(out.rewritten, "hello world");
        assert!(!out.changed);
        assert_eq!(out.token_count, 2);
    }

    #[test]
    fn vocabulary_with_only_far_candidates_keeps_identity() {
        // Vocabulary contains nothing within distance 2 of "hello".
        let vocab = vec!["completely_unrelated_xyzzy".to_string()];
        let out = rewrite_query("hello", 2, 1.0, 0.0, &vocab, None);
        assert_eq!(out.rewritten, "hello");
    }

    #[test]
    fn aggressive_edit_weight_prefers_close_candidate() {
        let vocab = vec!["receive".to_string()];
        // Negative edit weight makes corrections preferable; the LM
        // would normally do this — this test exercises the mechanism.
        let out = rewrite_query("recieve", 2, -1.0, 0.0, &vocab, None);
        assert_eq!(out.rewritten, "receive");
        assert!(out.changed);
    }

    #[test]
    fn tokenize_query_lowercases_and_drops_empties() {
        assert_eq!(
            tokenize_query("  Hello   WORLD  "),
            vec!["hello".to_string(), "world".to_string()]
        );
        assert!(tokenize_query("").is_empty());
        assert!(tokenize_query("    ").is_empty());
    }
}
