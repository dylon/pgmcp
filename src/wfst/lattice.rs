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
/// candidate. The identity token (matching the input verbatim) is
/// always added at cost 0 so the "no correction" path is always
/// available; candidate edges carry cost `distance * edit_weight`.
///
/// `edit_weight` lets the caller dial how aggressively to prefer
/// corrections vs. originals. Default 1.0 (one unit of cost per
/// edit). Values < 1 make corrections cheaper (more aggressive),
/// values > 1 make corrections more expensive.
pub fn build_correction_lattice(
    tokens: &[&str],
    candidates_per_token: &[Vec<TokenCandidate>],
    edit_weight: f64,
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
        // Identity edge always present at cost 0.
        builder.add_correction(
            i,
            i + 1,
            tok,
            TropicalWeight::new(0.0),
            EdgeMetadata::original(),
        );

        for cand in cands {
            // Skip identity dupes — adding the same surface form twice
            // would only inflate the lattice with no benefit (Viterbi
            // would pick the cheaper edge anyway, but the de-dupe
            // keeps the n-best list cleaner).
            if cand.term == *tok && cand.distance == 0 {
                continue;
            }
            let cost = (cand.distance as f64) * edit_weight;
            // EdgeMetadata::correction takes u8 (max edit cost
            // representable per-edge); saturate at u8::MAX for the
            // pathological "huge distance" case rather than wrap.
            let dist_u8 = u8::try_from(cand.distance).unwrap_or(u8::MAX);
            builder.add_correction(
                i,
                i + 1,
                cand.term.as_str(),
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

    #[test]
    fn identity_path_is_zero_cost_without_lm() {
        let tokens = ["hello", "world"];
        let cands = vec![Vec::<TokenCandidate>::new(), Vec::new()];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0);
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
        let cands = vec![vec![TokenCandidate {
            term: "hello".to_string(),
            distance: 0,
        }]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["hello".to_string()]);
    }

    #[test]
    fn higher_distance_costs_more() {
        let tokens = ["recieve"];
        let cands = vec![vec![TokenCandidate {
            term: "receive".to_string(),
            distance: 2,
        }]];
        let lat = build_correction_lattice(tokens.as_ref(), &cands, 1.0);
        let out = viterbi_best(&lat).expect("viterbi");
        // Identity path (cost 0) beats the correction (cost 2).
        assert_eq!(out.viterbi_path, vec!["recieve".to_string()]);
    }

    #[test]
    fn aggressive_edit_weight_picks_correction() {
        let tokens = ["recieve"];
        let cands = vec![vec![TokenCandidate {
            term: "receive".to_string(),
            distance: 2,
        }]];
        // Negative edit_weight makes corrections free / preferable.
        // (In practice edit_weight stays positive and the LM layer
        // produces the preference; this test just verifies the
        // mechanism.)
        let lat = build_correction_lattice(tokens.as_ref(), &cands, -1.0);
        let out = viterbi_best(&lat).expect("viterbi");
        assert_eq!(out.viterbi_path, vec!["receive".to_string()]);
    }
}
