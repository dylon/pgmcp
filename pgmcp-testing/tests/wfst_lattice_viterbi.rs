//! P13.2 — WFST lattice + Viterbi best-path determinism test.
//!
//! Builds a 3-token correction lattice with known candidates, runs
//! Viterbi without an LM, asserts the path matches the cheapest
//! candidate sequence and the cost is the sum of per-edge weights.
//!
//! The lattice builder takes `(tokens, candidates, edit_weight,
//! phonetic_cost_weight, phonetic_max_total_cost)`; these tests use
//! `0.0, 0.0` for the phonetic params (pure edit-distance behavior).

use pgmcp::wfst::lattice::{TokenCandidate, build_correction_lattice, viterbi_best};

fn cand(term: &str, distance: usize) -> TokenCandidate {
    TokenCandidate {
        term: term.to_string(),
        distance,
        phonetic_cost: 0.0,
    }
}

#[test]
fn empty_lattice_returns_empty_path_zero_cost() {
    let lat = build_correction_lattice(&[], &[], 1.0, 0.0, 0.0, false);
    let out = viterbi_best(&lat).expect("viterbi");
    assert!(out.viterbi_path.is_empty());
    assert!((out.viterbi_cost - 0.0).abs() < 1e-9);
}

#[test]
fn identity_path_preserves_input_tokens() {
    let tokens: [&str; 3] = ["alpha", "beta", "gamma"];
    let cands = vec![Vec::<TokenCandidate>::new(), Vec::new(), Vec::new()];
    let lat = build_correction_lattice(&tokens, &cands, 1.0, 0.0, 0.0, false);
    let out = viterbi_best(&lat).expect("viterbi");
    assert_eq!(
        out.viterbi_path,
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );
    assert!((out.viterbi_cost - 0.0).abs() < 1e-9);
}

#[test]
fn cheaper_candidate_displaces_identity() {
    let tokens: [&str; 2] = ["x", "y"];
    let cands = vec![vec![cand("X_better", 1)], Vec::new()];
    // Negative edit_weight makes corrections cheaper than identity
    // (cost 0). This exercises the min-selection path; in production
    // the same effect is produced by the LM layer driving correction
    // edges to lower scores than identity.
    let lat = build_correction_lattice(&tokens, &cands, -1.0, 0.0, 0.0, false);
    let out = viterbi_best(&lat).expect("viterbi");
    assert_eq!(
        out.viterbi_path,
        vec!["X_better".to_string(), "y".to_string()]
    );
    // Cost = -1 (correction) + 0 (identity at pos 1) = -1
    assert!((out.viterbi_cost - (-1.0)).abs() < 1e-9);
}

#[test]
fn viterbi_is_deterministic_for_identical_calls() {
    let tokens: [&str; 2] = ["foo", "bar"];
    let cands = vec![vec![cand("fooz", 1)], vec![cand("barr", 1)]];
    let lat1 = build_correction_lattice(&tokens, &cands, 1.0, 0.0, 0.0, false);
    let lat2 = build_correction_lattice(&tokens, &cands, 1.0, 0.0, 0.0, false);
    let out1 = viterbi_best(&lat1).expect("viterbi-1");
    let out2 = viterbi_best(&lat2).expect("viterbi-2");
    assert_eq!(out1.viterbi_path, out2.viterbi_path);
    assert!((out1.viterbi_cost - out2.viterbi_cost).abs() < 1e-9);
}
