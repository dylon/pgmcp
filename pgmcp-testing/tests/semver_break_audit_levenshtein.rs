//! Regression baseline for Phase 3 of the integration plan
//! `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.
//!
//! `src/mcp/tools/tool_semver_break_audit.rs` was migrated from
//! `strsim::levenshtein` (brute-force O(N×L²) scan) to
//! `liblevenshtein::Transducer::with_transposition` over a
//! `libdictenstein::DynamicDawgChar`. The transducer's rename-candidate
//! selection must agree with the previous strsim behavior for all
//! historical fixtures the tool ever surfaced.
//!
//! This test asserts the new path produces the same `likely_rename_to`
//! pick on a representative grid of (removed_name, now_names) cases
//! that the legacy strsim implementation would have returned. Two
//! domains:
//!
//! 1. The library-layer behavior (the `Transducer.query_with_distance(.., 2)
//!    .min_by_key(|c| c.distance)` rewrite) — no DB needed.
//! 2. Distance-1 transposition (Damerau-Levenshtein) cases:
//!    `teh` ↔ `the`, `recieve` ↔ `receive`. `with_transposition` treats
//!    adjacent swaps as a single edit, matching the documented behavior.

use libdictenstein::Dictionary;
use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;

/// Build the same dictionary + transducer pair the tool builds at runtime.
fn make_transducer(now_names: &[&str]) -> Transducer<DynamicDawgChar<()>> {
    let dict = DynamicDawgChar::from_terms(now_names.to_vec());
    Transducer::with_transposition(dict)
}

/// Run the Phase-3 rewrite logic (`query_with_distance + min_by_key`) and
/// return the picked rename, if any.
fn likely_rename(transducer: &Transducer<DynamicDawgChar<()>>, removed: &str) -> Option<String> {
    transducer
        .query_with_distance(removed, 2)
        .min_by_key(|c| c.distance)
        .map(|c| c.term)
}

#[test]
fn rename_picks_closest_at_distance_one() {
    let now = ["the", "their", "there"];
    let t = make_transducer(&now);
    // `teh` is one Damerau-Levenshtein edit (transposition) from `the`;
    // ≥ 2 edits from `their` / `there`. Closest = `the`.
    assert_eq!(likely_rename(&t, "teh"), Some("the".to_string()));
}

#[test]
fn rename_picks_closest_at_distance_two_substitution() {
    let now = ["receive", "recipe", "decide"];
    let t = make_transducer(&now);
    // `recieve` is one transposition from `receive` (i↔e adjacent swap)
    // — closest match at distance 1.
    assert_eq!(likely_rename(&t, "recieve"), Some("receive".to_string()));
}

#[test]
fn rename_returns_none_when_no_candidate_within_two_edits() {
    let now = ["antidisestablishment"];
    let t = make_transducer(&now);
    // `cat` has nothing within distance 2 in this dictionary.
    assert_eq!(likely_rename(&t, "cat"), None);
}

#[test]
fn rename_handles_empty_dictionary() {
    let now: [&str; 0] = [];
    let t = make_transducer(&now);
    assert_eq!(likely_rename(&t, "anything"), None);
}

#[test]
fn rename_returns_first_at_min_distance_when_multiple_tied() {
    // Two terms at distance 1; min_by_key picks one. The actual choice is
    // implementation-defined (`min_by_key` keeps the first seen on a tie),
    // but the picked term must be one of the tied candidates.
    let now = ["bat", "cat"];
    let t = make_transducer(&now);
    let pick = likely_rename(&t, "rat");
    assert!(
        matches!(pick.as_deref(), Some("bat" | "cat")),
        "expected `bat` or `cat`, got {:?}",
        pick
    );
}

#[test]
fn rename_treats_transposition_as_distance_one() {
    // Damerau-Levenshtein: swap of adjacent characters is one edit, not
    // two (which plain Levenshtein would charge). `with_transposition`
    // is the explicit constructor that enables this behavior.
    let now = ["abcd"];
    let t = make_transducer(&now);
    // `bacd` is one swap from `abcd`.
    let pick = likely_rename(&t, "bacd");
    assert_eq!(pick, Some("abcd".to_string()));
}

#[test]
fn rename_caps_at_distance_two() {
    // `kitten` ↔ `sitting` is the classical edit-distance-3 example.
    // With max_distance = 2, no match should be returned.
    let now = ["sitting"];
    let t = make_transducer(&now);
    assert_eq!(likely_rename(&t, "kitten"), None);
}

#[test]
fn dictionary_size_matches_unique_input_terms() {
    let now = ["foo", "bar", "foo", "baz"];
    let dict = DynamicDawgChar::<()>::from_terms(now.to_vec());
    // DAWG dedupes by term; size == unique input set.
    assert_eq!(dict.len().unwrap_or(0), 3);
}
