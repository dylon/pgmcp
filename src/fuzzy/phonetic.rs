//! Wiring for `liblevenshtein`'s phonetic framework into pgmcp.
//!
//! This module hosts the shared helpers used by Phase 10 and by the
//! phonetic MCP tools (Phase 8): `phonetic_normalize`,
//! `phonetic_symbol_search`, `phonetic_grep_comments`, `token_grep`,
//! `expand_query_to_phonetic_pattern`, `phonetic_naming_consistency`,
//! `articulatory_naming_consistency`.

use liblevenshtein::phonetic::feature_distance::{
    articulatory_distance, articulatory_edit_distance,
};

/// Articulatory edit distance — Levenshtein with per-character
/// substitution costs from the IPA articulatory-feature table
/// (`articulatory_distance`). `'p'` vs `'b'` ≈ 0.1 (voicing only);
/// `'p'` vs `'f'` ≈ 0.3 (manner change); `'a'` vs `'p'` = 1.0
/// (vowel vs consonant). pgmcp uses this in place of 0/1 Levenshtein
/// wherever identifier-similarity scoring is more useful with
/// linguistically-meaningful character costs (naming consistency,
/// duplicate detection, rename detection).
pub fn articulatory_distance_score(a: &str, b: &str) -> f64 {
    articulatory_edit_distance(a, b)
}

/// Per-character articulatory distance (forwarded for callers that
/// want pairwise comparisons).
pub fn char_articulatory_distance(a: char, b: char) -> f64 {
    articulatory_distance(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voicing_pair_is_cheaper_than_place_change() {
        // p ↔ b is voicing-only: cheap.
        let v = char_articulatory_distance('p', 'b');
        // p ↔ k is voicing same, place very different (bilabial → velar).
        let p = char_articulatory_distance('p', 'k');
        assert!(
            v <= p,
            "voicing-only ({}) should be ≤ place change ({})",
            v,
            p
        );
    }

    #[test]
    fn articulatory_word_distance_picks_up_voicing_swap() {
        let same = articulatory_distance_score("path", "path");
        let close = articulatory_distance_score("path", "bath");
        let far = articulatory_distance_score("path", "math");
        assert_eq!(same, 0.0);
        assert!(close > same);
        assert!(far > 0.0);
    }
}
