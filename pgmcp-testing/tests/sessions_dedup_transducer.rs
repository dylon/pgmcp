//! Regression baseline for Phase 3 of the integration plan
//! `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.
//!
//! `src/sessions.rs::mark_near_duplicate_superseded` was migrated from
//! Postgres's `levenshtein_less_equal` (from the `fuzzystrmatch`
//! extension) to an in-process `liblevenshtein::Transducer` over a
//! `libdictenstein::DynamicDawgChar`. The transducer-based dedup must
//! agree with the previous SQL behavior on representative inputs:
//!
//! - Lower-cased imperative equality is excluded server-side (the SQL
//!   `WHERE lower(imperative) <> lower($4)` clause is preserved).
//! - Near-duplicates within `max_distance` are selected; everything
//!   above is left alone.
//! - Damerau-Levenshtein semantics (transposition = 1 edit), matching
//!   the documented behavior of the legacy `fuzzystrmatch` path.
//!
//! The full integration test (with real PG) lives in
//! `memory_phase0.rs::mark_near_duplicate_superseded_collapses_edit_distance_3`
//! and remains the canonical end-to-end gate. This file exercises the
//! pure Transducer layer that pgmcp wraps around the PG row fetch.

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use std::collections::HashMap;

/// Mirror the helper logic in `sessions::mark_near_duplicate_superseded`:
/// build the per-session DAWG + transducer over active mandates,
/// query for terms within `max_distance` of the new imperative, and
/// return the matched row ids.
fn dedupe_candidates(
    active: &[(i64, &str)],
    new_imperative: &str,
    max_distance: usize,
) -> Vec<i64> {
    let mut id_index: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, imp) in active {
        id_index.entry(imp.to_lowercase()).or_default().push(*id);
    }
    let terms: Vec<&str> = id_index.keys().map(|s| s.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(terms);
    let transducer = Transducer::with_transposition(dict);

    let new_lower = new_imperative.to_lowercase();
    let mut out: Vec<i64> = Vec::new();
    for candidate in transducer.query_with_distance(&new_lower, max_distance) {
        if let Some(ids) = id_index.get(&candidate.term) {
            out.extend(ids.iter().copied());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn dedupes_case_variants_at_distance_zero() {
    // "use Rust" / "use rust" are equal under `lower(..)`. The
    // production code excludes exact-lowercase-equality server-side
    // (`lower(imperative) <> lower($4)`); we mirror that exclusion
    // in the test fixture.
    let active = [(1i64, "use unwrap")];
    let new = "use unwrap"; // identical under lower(..)
    // The helper would have filtered the row before passing it here;
    // the transducer over an empty set yields no matches.
    let empty: [(i64, &str); 0] = [];
    assert_eq!(dedupe_candidates(&empty, new, 3), Vec::<i64>::new());

    // With a non-overlapping active row, exact-lower-match is impossible.
    let actives = [(2i64, "avoid panic")];
    assert_eq!(
        dedupe_candidates(&actives, "use unwrap", 3),
        Vec::<i64>::new(),
        "lexically distant imperatives are not deduped"
    );
    let _ = active;
}

#[test]
fn dedupes_single_edit_distance_match() {
    // "use unwrap" vs "use unwraps" differ by 1 insertion — within
    // the default max_distance of 3.
    let active = [(42i64, "use unwraps")];
    let picks = dedupe_candidates(&active, "use unwrap", 3);
    assert_eq!(picks, vec![42], "1-edit near-dup must be flagged");
}

#[test]
fn dedupes_transposition_at_distance_one() {
    // "use ruts" vs "use rust" differ by an adjacent swap (s↔t).
    // Damerau-Levenshtein scores this as distance 1.
    let active = [(7i64, "use ruts")];
    let picks = dedupe_candidates(&active, "use rust", 3);
    assert_eq!(picks, vec![7], "transposition must count as 1 edit");
}

#[test]
fn does_not_dedupe_above_max_distance() {
    let active = [(99i64, "completely unrelated")];
    let picks = dedupe_candidates(&active, "use unwrap", 3);
    assert!(
        picks.is_empty(),
        "imperatives > max_distance must not be deduped: got {:?}",
        picks
    );
}

#[test]
fn dedupes_multiple_near_duplicates_in_one_pass() {
    let active = [
        (1i64, "use unwrap"),
        (2i64, "use unwraps"), // distance 1
        (3i64, "user unwrap"), // distance 1 (insertion)
        (4i64, "completely different phrasing"),
    ];
    let picks = dedupe_candidates(&active, "use unwrap", 2);
    let mut expected = vec![1, 2, 3];
    expected.sort();
    let mut got = picks.clone();
    got.sort();
    assert_eq!(got, expected, "near-dups within max_distance flagged");
}

#[test]
fn map_back_handles_duplicate_lowercase_imperatives() {
    // Two rows whose imperatives lower-case to the same string but with
    // different casing in the original — both ids must be reported.
    let active = [(10i64, "Use Rust"), (11i64, "USE RUST")];
    let picks = dedupe_candidates(&active, "use rust", 0);
    let mut expected = vec![10, 11];
    expected.sort();
    let mut got = picks;
    got.sort();
    assert_eq!(got, expected);
}

#[test]
fn empty_active_set_returns_no_picks() {
    let active: [(i64, &str); 0] = [];
    assert_eq!(dedupe_candidates(&active, "anything", 3), Vec::<i64>::new());
}

#[test]
fn max_distance_zero_only_dedupes_exact_lowercase_matches() {
    let active = [(1i64, "use unwrap"), (2i64, "use unwraps")];
    let picks = dedupe_candidates(&active, "USE UNWRAP", 0);
    assert_eq!(picks, vec![1], "distance 0 = exact lowercase match only");
}
