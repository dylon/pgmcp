//! Query rewriting via WFST lattice + HybridLM rescoring.
//!
//! Entry point used by both `tool_hybrid_search` (third RRF leg) and
//! `tool_correct_query` (single-shot user-facing correction). The
//! function takes a free-form query, tokenizes it, generates
//! Damerau-Levenshtein candidates from the per-project persistent
//! `FuzzyIndex` (P14.5 — replaces the prior in-memory
//! `DynamicDawgChar::from_terms(vocabulary)` rebuild-per-call),
//! builds a TropicalWeight lattice, optionally rescores with the
//! per-project HybridLM, and returns the Viterbi-best rewritten
//! query.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2 + Phase 14.5.

use libdictenstein::DictionaryValue;

use super::hybrid_lm::PgmcpHybridLm;
use super::lattice::{TokenCandidate, build_correction_lattice, rescore_with_lm, viterbi_best};
use crate::fuzzy::persistent_artrie::FuzzyIndex;
use crate::fuzzy::phonetic::articulatory_distance_score;

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
/// `fuzzy_idx` is the per-project persistent `FuzzyIndex` (typically
/// `FuzzyIndex<SymbolValue>` from `crate::fuzzy::sync::open_symbol_trie`).
/// Per-token candidates are pulled via `idx.query(tok, max_distance)`
/// directly from the on-disk `PersistentARTrieChar` — no per-call
/// DAWG rebuild.
///
/// `lm_weight` interpolates LM cost into the lattice edge weights
/// (0.0 = ignore LM, 1.0 = LM only). When `lm` is `None` the LM step
/// is skipped entirely — the returned `RewrittenQuery.used_lm` field
/// signals that.
#[allow(clippy::too_many_arguments)]
pub fn rewrite_query<V>(
    query: &str,
    max_distance: usize,
    edit_weight: f64,
    lm_weight: f64,
    phonetic_cost_weight: f64,
    phonetic_max_total_cost: f64,
    fuzzy_idx: &FuzzyIndex<V>,
    lm: Option<&PgmcpHybridLm>,
) -> RewrittenQuery
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    // Split on whitespace WITHOUT lowercasing. The persistent symbol trie
    // stores symbols in their original case and matches case-sensitively,
    // so the trie must be queried with the original surface form — mirroring
    // `fuzzy_symbol_search`. Lowercasing first (as the generic
    // `tokenize_query` does) would inflate the edit distance of every case
    // difference and hide a mixed-case symbol behind `max_distance` (e.g.
    // lowercased "chunkerinpt" is distance 3 from "ChunkerInput", so it is
    // dropped at the default max_distance=2 and never corrected).
    let surface_tokens: Vec<&str> = query.split_whitespace().collect();
    if surface_tokens.is_empty() {
        return RewrittenQuery {
            original: query.to_string(),
            rewritten: query.to_string(),
            changed: false,
            token_count: 0,
            used_lm: false,
        };
    }

    // Per-token candidates come straight from the persistent trie.
    // When the trie is empty the query returns Vec::new() → no
    // candidates → identity path wins.
    let candidates_per_token: Vec<Vec<TokenCandidate>> = surface_tokens
        .iter()
        .map(|&tok| {
            fuzzy_idx
                .query(tok, max_distance)
                .into_iter()
                .map(|(term, distance, _value)| {
                    // Phonetic cost = articulatory distance between the input
                    // token and the candidate; blended into the lattice edge
                    // cost so phonetically-closer corrections are preferred.
                    let phonetic_cost = articulatory_distance_score(tok, &term);
                    TokenCandidate {
                        term,
                        distance,
                        phonetic_cost,
                    }
                })
                .collect()
        })
        .collect();

    // `oov_autocorrect = true`: commit edit/phonetic corrections for genuine
    // out-of-vocabulary typos even when no LM rescores the lattice (in-vocab
    // tokens are never over-corrected). See `build_correction_lattice`.
    let base_lattice = build_correction_lattice(
        &surface_tokens,
        &candidates_per_token,
        edit_weight,
        phonetic_cost_weight,
        phonetic_max_total_cost,
        true,
    );

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
                token_count: surface_tokens.len(),
                used_lm,
            };
        }
    };

    let rewritten = out.viterbi_path.join(" ");
    let changed = rewritten != surface_tokens.join(" ");
    RewrittenQuery {
        original: query.to_string(),
        rewritten,
        changed,
        token_count: surface_tokens.len(),
        used_lm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only helper: build an empty in-memory `FuzzyIndex<()>`
    /// in a tempdir for unit tests. The test owns the `tempfile`
    /// directory; dropping it cleans up the trie files on disk.
    fn empty_trie() -> (tempfile::TempDir, FuzzyIndex<()>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.artrie");
        let (idx, _recovery) = FuzzyIndex::<()>::open_or_create(&path).expect("create");
        (tmp, idx)
    }

    /// Test-only helper: build a `FuzzyIndex<()>` pre-populated
    /// with the given terms.
    fn trie_with(terms: &[&str]) -> (tempfile::TempDir, FuzzyIndex<()>) {
        let (tmp, idx) = empty_trie();
        for term in terms {
            idx.upsert(term, ()).expect("upsert");
        }
        (tmp, idx)
    }

    #[test]
    fn empty_query_returns_unchanged() {
        let (_tmp, idx) = empty_trie();
        let out = rewrite_query("", 2, 1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(out.original, "");
        assert_eq!(out.rewritten, "");
        assert_eq!(out.token_count, 0);
        assert!(!out.changed);
    }

    #[test]
    fn empty_vocabulary_identity_pass_through() {
        let (_tmp, idx) = empty_trie();
        let out = rewrite_query("hello world", 2, 1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(out.rewritten, "hello world");
        assert!(!out.changed);
        assert_eq!(out.token_count, 2);
    }

    #[test]
    fn vocabulary_with_only_far_candidates_keeps_identity() {
        let (_tmp, idx) = trie_with(&["completely_unrelated_xyzzy"]);
        let out = rewrite_query("hello", 2, 1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(out.rewritten, "hello");
    }

    #[test]
    fn aggressive_edit_weight_prefers_close_candidate() {
        let (_tmp, idx) = trie_with(&["receive"]);
        // Negative edit weight makes corrections preferable; the LM
        // would normally do this — this test exercises the mechanism.
        let out = rewrite_query("recieve", 2, -1.0, 0.0, 0.0, 0.0, &idx, None);
        assert_eq!(out.rewritten, "receive");
        assert!(out.changed);
    }

    #[test]
    fn phonetic_weighting_threads_through_and_still_corrects() {
        let (_tmp, idx) = trie_with(&["receive"]);
        // phonetic_cost_weight > 0 with a generous cap: the strongly-preferred
        // correction (edit_weight -10) still wins after the phonetic term is
        // blended in — i.e. the phonetic params are threaded without breaking
        // a legitimate near-miss correction.
        let out = rewrite_query("recieve", 2, -10.0, 0.0, 1.0, 100.0, &idx, None);
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

    #[test]
    fn oov_token_corrects_with_default_weight() {
        // The default (production) edit_weight = 1.0 with no LM must still
        // correct an OOV typo — the Bug-1 fix at the rewrite_query layer.
        let (_tmp, idx) = trie_with(&["receive"]);
        let out = rewrite_query("recieve", 2, 1.0, 0.0, 0.0, 3.0, &idx, None);
        assert_eq!(out.rewritten, "receive");
        assert!(out.changed);
    }

    #[test]
    fn correctly_typed_mixedcase_not_changed() {
        // A correctly-typed mixed-case symbol is in-vocab (distance-0 exact
        // match) → not over-corrected. Also proves the trie is queried in
        // original case (Option A): a lowercasing query path would never see
        // a distance-0 match here.
        let (_tmp, idx) = trie_with(&["ChunkerInput"]);
        let out = rewrite_query("ChunkerInput", 2, 1.0, 0.0, 0.0, 3.0, &idx, None);
        assert_eq!(out.rewritten, "ChunkerInput");
        assert!(!out.changed);
    }

    #[test]
    fn oov_mixedcase_corrects() {
        // Regression for the camelCase repro: "ChunkerInpt" is distance 1
        // from "ChunkerInput" ONLY when queried in original case. The fix
        // queries the trie with the original surface, so the candidate is
        // found and committed.
        let (_tmp, idx) = trie_with(&["ChunkerInput"]);
        let out = rewrite_query("ChunkerInpt", 2, 1.0, 0.0, 0.0, 3.0, &idx, None);
        assert_eq!(out.rewritten, "ChunkerInput");
        assert!(out.changed);
    }
}
