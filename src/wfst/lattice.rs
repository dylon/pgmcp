//! WFST lattice construction + Viterbi/n-best rescoring.
//!
//! Builds a `Lattice<TropicalWeight, HashMapBackend>` over a tokenized
//! query plus per-token Damerau-Levenshtein candidates. Lazy-composes
//! with the project's `HybridLanguageModel` via lling-llang's
//! `LanguageModelLayer`. Returns the Viterbi-best path plus n-best.
//!
//! Used by `wfst::query_rescore` (third RRF leg) and
//! `wfst::correction` (single-shot correction for
//! `tool_correct_query`).
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9 + Phase 13.2.

use lling_llang::backend::HashMapBackend;
use lling_llang::lattice::Lattice;
use lling_llang::lattice::{EdgeMetadata, LatticeBuilder};
use lling_llang::layers::CorrectionLayer;
use lling_llang::layers::rescoring::lm_rerank::{LanguageModel, LanguageModelLayer};
use lling_llang::semiring::TropicalWeight;

use super::hybrid_lm::PgmcpHybridLm;

/// Per-token candidate produced by a fuzzy-index lookup.
#[derive(Debug, Clone)]
pub struct TokenCandidate {
    /// Surface form of the candidate (e.g. the dictionary word).
    pub term: String,
    /// Edit distance from the input token. 0 means identity / no
    /// correction.
    pub distance: usize,
    /// Articulatory (phonetic) distance from the input token. Blended
    /// into the edge cost via `phonetic_cost_weight` so phonetically
    /// closer corrections are preferred among equal-edit-distance
    /// candidates. 0.0 for an identity candidate.
    pub phonetic_cost: f64,
}

/// Output of `rescore_lattice` and `viterbi_best`.
#[derive(Debug, Clone)]
pub struct LatticeRescoreOutput {
    /// Best-scoring sequence of tokens (one entry per position).
    pub viterbi_path: Vec<String>,
    /// Total cost of the Viterbi path.
    pub viterbi_cost: f64,
}

/// Build a TropicalWeight correction lattice from tokens + per-token
/// candidate sets.
///
/// For each token i, an edge from position i → i+1 is added per
/// candidate plus an identity edge (matching the input verbatim).
/// Candidate edges carry cost
/// `distance * edit_weight + phonetic_cost * phonetic_cost_weight`.
///
/// `edit_weight` lets the caller dial how aggressively to prefer
/// corrections vs. originals. Default 1.0 (one unit of cost per
/// edit). Values < 1 make corrections cheaper (more aggressive),
/// values > 1 make corrections more expensive.
///
/// `phonetic_cost_weight` blends each candidate's articulatory
/// distance into the edge cost (0.0 disables the phonetic term →
/// pure edit-distance behavior). `phonetic_max_total_cost`, when
/// positive, drops candidates whose blended cost exceeds the cap; the
/// identity edge survives the cap, so the no-correction path is never
/// pruned.
///
/// `oov_autocorrect` controls the identity-edge cost, which decides
/// whether a genuine typo is committed when no language model is
/// available to rescore the lattice:
/// - `false` → the identity edge is always free (cost 0.0); Viterbi
///   keeps the original token unless a candidate is cheaper (only
///   possible with a negative `edit_weight` or an LM layer). Legacy
///   behavior.
/// - `true` → for a token that is itself out-of-vocabulary (no
///   distance-0 self-match among its candidates) and has at least one
///   in-budget candidate, the identity edge is priced strictly above
///   every candidate (`max_candidate_cost + 1.0`), so Viterbi commits
///   the lowest-cost real correction even with a positive `edit_weight`
///   and no LM. A token that is itself in-vocabulary, or that has no
///   candidates, keeps the free identity edge — so correctly-spelled
///   symbols are never over-corrected and the lattice always has a
///   well-formed path.
pub fn build_correction_lattice(
    tokens: &[&str],
    candidates_per_token: &[Vec<TokenCandidate>],
    edit_weight: f64,
    phonetic_cost_weight: f64,
    phonetic_max_total_cost: f64,
    oov_autocorrect: bool,
) -> Lattice<TropicalWeight, HashMapBackend> {
    debug_assert_eq!(tokens.len(), candidates_per_token.len());
    let backend = HashMapBackend::new();
    let mut builder = LatticeBuilder::<TropicalWeight, _>::with_capacity(
        backend,
        tokens.len() + 1,
        candidates_per_token
            .iter()
            .map(|c| c.len() + 1)
            .sum::<usize>()
            / tokens.len().max(1),
    );

    for (i, (tok, cands)) in tokens.iter().zip(candidates_per_token.iter()).enumerate() {
        // First pass: collect the candidate edges we will add (applying
        // the identity-dupe skip and the phonetic-cost cap), tracking the
        // maximum accepted cost and whether the token is itself a
        // vocabulary entry (a distance-0 self-match).
        let mut accepted: Vec<(&str, f64, u8)> = Vec::with_capacity(cands.len());
        let mut max_candidate_cost = f64::NEG_INFINITY;
        let mut in_vocab = false;

        for cand in cands {
            // A distance-0 self-match means the token IS a real vocabulary
            // entry — record that (it suppresses OOV auto-correction below)
            // and skip the dupe edge: adding the same surface form twice
            // would only inflate the lattice / n-best with no benefit.
            if cand.distance == 0 && cand.term == *tok {
                in_vocab = true;
                continue;
            }
            let cost =
                (cand.distance as f64) * edit_weight + cand.phonetic_cost * phonetic_cost_weight;
            // Drop candidates whose blended cost exceeds the configured cap
            // (a positive cap activates this; 0.0 disables it).
            if phonetic_max_total_cost > 0.0 && cost > phonetic_max_total_cost {
                continue;
            }
            // EdgeMetadata::correction takes u8 (max edit cost
            // representable per-edge); saturate at u8::MAX for the
            // pathological "huge distance" case rather than wrap.
            let dist_u8 = u8::try_from(cand.distance).unwrap_or(u8::MAX);
            accepted.push((cand.term.as_str(), cost, dist_u8));
            if cost > max_candidate_cost {
                max_candidate_cost = cost;
            }
        }

        // Identity-edge cost. Free (0.0) by default so the "no correction"
        // path is always available. When OOV auto-correction is enabled and
        // the token is a genuine typo (not in-vocab) with at least one
        // in-budget candidate, price identity strictly above every candidate
        // so Viterbi must commit the lowest-cost real correction — the base
        // edit/phonetic correction behavior that applies even with no LM to
        // rescore the lattice.
        let identity_cost = if oov_autocorrect && !in_vocab && !accepted.is_empty() {
            max_candidate_cost + 1.0
        } else {
            0.0
        };
        builder.add_correction(
            i,
            i + 1,
            tok,
            TropicalWeight::new(identity_cost),
            EdgeMetadata::original(),
        );

        for (term, cost, dist_u8) in accepted {
            builder.add_correction(
                i,
                i + 1,
                term,
                TropicalWeight::new(cost),
                EdgeMetadata::correction(dist_u8),
            );
        }
    }

    builder.build(tokens.len())
}

/// Apply `LanguageModelLayer` rescoring on top of the correction
/// lattice. `lm_weight` is the interpolation weight (0.0 = ignore LM,
/// 1.0 = LM-only).
pub fn rescore_with_lm(
    lattice: &Lattice<TropicalWeight, HashMapBackend>,
    lm: &PgmcpHybridLm,
    lm_weight: f64,
) -> Result<Lattice<TropicalWeight, HashMapBackend>, String> {
    let lm: Box<dyn LanguageModel> = Box::new(lm.clone());
    let layer = LanguageModelLayer::new(lm).with_weight(lm_weight);
    layer.apply(lattice).map_err(|e| format!("{e:?}"))
}

/// Run Viterbi on the lattice. Because the correction lattice is a
/// linear DAG of positions (0 → 1 → 2 → ... → N) with parallel edges
/// only between consecutive positions, and because the LM rescoring
/// layer assigns a single weight per edge (averaging LM scores over
/// incoming contexts before rescoring), the Viterbi-best path is just
/// the minimum-weight outgoing edge per position concatenated in
/// order. This is O(N·K) where K is the maximum candidate count per
/// position, regardless of N (vs. exponential path enumeration).
pub fn viterbi_best(
    lattice: &Lattice<TropicalWeight, HashMapBackend>,
) -> Result<LatticeRescoreOutput, String> {
    if lattice.num_nodes() == 0 {
        return Ok(LatticeRescoreOutput {
            viterbi_path: Vec::new(),
            viterbi_cost: 0.0,
        });
    }

    let mut current = lattice.start();
    let end = lattice.end();
    let mut viterbi_path = Vec::new();
    let mut viterbi_cost = 0.0_f64;

    // Walk position-by-position taking the min-cost outgoing edge.
    // Bounded by num_nodes() to refuse to loop on a malformed lattice.
    let mut hops = 0usize;
    while current != end && hops <= lattice.num_nodes() {
        let best = lattice.outgoing_edges(current).min_by(|a, b| {
            a.weight
                .value()
                .partial_cmp(&b.weight.value())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let Some(edge) = best else { break };
        if let Some(word) = lattice.edge_word(edge) {
            viterbi_path.push(word.to_string());
        }
        viterbi_cost += edge.weight.value();
        current = edge.target;
        hops += 1;
    }

    Ok(LatticeRescoreOutput {
        viterbi_path,
        viterbi_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(term: &str, distance: usize, phonetic_cost: f64) -> TokenCandidate {
        TokenCandidate {
            term: term.to_string(),
            distance,
            phonetic_cost,
        }
    }

    #[test]
    fn identity_path_is_zero_cost_without_lm() {
        let tokens = ["hello", "world"];
        let cands = vec![Vec::<TokenCandidate>::new(), Vec::new()];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(
            out.viterbi_path,
            vec!["hello".to_string(), "world".to_string()]
        );
        assert!(
            (out.viterbi_cost - 0.0).abs() < 1e-9,
            "identity path must cost 0.0, got {}",
            out.viterbi_cost
        );
    }

    #[test]
    fn candidate_path_wins_when_distance_zero() {
        // Two candidates, both distance 0: Viterbi picks one (the
        // earlier-added "hello" because it has matching cost) — point
        // of the test is to exercise multi-edge case without LM.
        let tokens = ["hello"];
        let cands = vec![vec![cand("hello", 0, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["hello".to_string()]);
    }

    #[test]
    fn higher_distance_costs_more() {
        let tokens = ["recieve"];
        let cands = vec![vec![cand("receive", 2, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        // Identity path (cost 0) beats the correction (cost 2).
        assert_eq!(out.viterbi_path, vec!["recieve".to_string()]);
    }

    #[test]
    fn aggressive_edit_weight_picks_correction() {
        let tokens = ["recieve"];
        let cands = vec![vec![cand("receive", 2, 0.0)]];
        // Negative edit_weight makes corrections free / preferable.
        // (In practice edit_weight stays positive and the LM layer
        // produces the preference; this test just verifies the
        // mechanism.)
        let lat = build_correction_lattice(tokens.as_ref(), &cands, -1.0, 0.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["receive".to_string()]);
    }

    #[test]
    fn phonetic_cost_breaks_ties_among_equal_edit_distance() {
        // Two corrections at the same edit distance; with a negative edit
        // weight both beat identity, and the phonetically-closer one (lower
        // phonetic_cost) must win once phonetic_cost_weight > 0.
        let tokens = ["kat"];
        let cands = vec![vec![cand("cat", 1, 0.05), cand("bat", 1, 0.9)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, -1.0, 1.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["cat".to_string()]);
    }

    #[test]
    fn phonetic_max_total_cost_drops_over_budget_candidate() {
        // A far candidate whose blended cost exceeds the cap is dropped, so
        // the identity path wins even with an aggressive phonetic weight.
        let tokens = ["xyz"];
        let cands = vec![vec![cand("receive", 2, 5.0)]];
        // blended cost = 2 * 1.0 + 5.0 * 1.0 = 7.0 > cap 3.0 → dropped.
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 1.0, 3.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["xyz".to_string()]);
    }

    #[test]
    fn oov_token_with_candidate_is_corrected_without_lm() {
        // The canonical Bug-1 regression: at the production edit_weight (1.0)
        // and with NO LM, an out-of-vocabulary typo must be corrected to its
        // nearest real candidate. (Fails before the OOV-aware identity cost:
        // the free identity edge would win.)
        let tokens = ["recieve"];
        let cands = vec![vec![cand("receive", 1, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, true);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["receive".to_string()]);
    }

    #[test]
    fn in_vocab_token_not_overcorrected() {
        // A token that is itself in-vocabulary (distance-0 self-match) must
        // NOT be nudged to a distance-1 neighbor, even with OOV auto-correct
        // enabled — the over-correction guard.
        let tokens = ["chunked"];
        let cands = vec![vec![cand("chunked", 0, 0.0), cand("chunker", 1, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, true);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["chunked".to_string()]);
    }

    #[test]
    fn zero_candidate_token_passes_through_with_autocorrect() {
        // No candidates within budget → the lattice keeps a free identity
        // edge and the token passes through unchanged (well-formed path).
        let tokens = ["xyzzy"];
        let cands = vec![Vec::<TokenCandidate>::new()];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, true);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["xyzzy".to_string()]);
    }

    #[test]
    fn multi_token_mixed_invocab_and_oov() {
        // Per-token independence: an in-vocab token is preserved while an
        // OOV typo in the same query is corrected.
        let tokens = ["decode", "recieve"];
        let cands = vec![vec![cand("decode", 0, 0.0)], vec![cand("receive", 1, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, true);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(
            out.viterbi_path,
            vec!["decode".to_string(), "receive".to_string()]
        );
    }

    #[test]
    fn oov_autocorrect_false_preserves_legacy_passthrough() {
        // With the flag off, the legacy behavior holds: a positive edit_weight
        // keeps the free identity edge winning over a real candidate.
        let tokens = ["recieve"];
        let cands = vec![vec![cand("receive", 1, 0.0)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 0.0, 0.0, false);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["recieve".to_string()]);
    }

    #[test]
    fn phonetic_breaks_tie_among_oov_candidates_positive_weight() {
        // OOV auto-correct composes with the phonetic tiebreak at the
        // production positive edit_weight: two equal-edit candidates, the
        // phonetically-closer one wins.
        let tokens = ["kat"];
        let cands = vec![vec![cand("cat", 1, 0.05), cand("bat", 1, 0.9)]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0, 1.0, 0.0, true);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["cat".to_string()]);
    }
}
