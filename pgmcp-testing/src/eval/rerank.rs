//! Second-stage rerankers for the eval harness: the BGE-reranker-v2-m3
//! cross-encoder and BGE-M3 ColBERT MaxSim late-interaction.
//!
//! Both re-score the top-N candidates from `semantic_search` and reorder them,
//! so the campaign can measure the ranking lift over plain dense retrieval (the
//! known-item `Success@1 ≈ 0.14` leaves clear top-rank headroom). This mirrors
//! the `/api/search` rerank pipeline (`src/api/handlers.rs`) but standalone.
//!
//! Fully local. The cross-encoder is GPU-greedy with no `use_gpu` knob (it tries
//! CUDA(0) then silently falls back to CPU); run the campaign bin with
//! `CUDA_VISIBLE_DEVICES=""` to force CPU deterministically alongside the live
//! daemon, which holds most of the card.

use pgmcp::embed::model::{Embedder, colbert_maxsim};
use pgmcp::quality::retrieval_metrics::RankedHit;
use pgmcp::reranker::Reranker;

/// A retrieval candidate carrying its passage text — rerankers need the text,
/// not just the path/score that the metric layer keys on.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub hit: RankedHit,
    pub content: String,
}

impl Candidate {
    /// Convenience for tests/fixtures.
    pub fn new(path: &str, content: &str) -> Self {
        Self {
            hit: RankedHit::path_only(path),
            content: content.to_string(),
        }
    }
}

/// Cross-encoder rerank: re-score each `(query, content)` pair with the
/// BGE-reranker-v2-m3 single-label head, return the candidates reordered
/// (highest relevance first) and truncated to `limit`. The returned hits carry
/// the sigmoid relevance as their `score`.
pub fn cross_encoder_rerank(
    reranker: &dyn Reranker,
    query: &str,
    cands: &[Candidate],
    limit: usize,
) -> Result<Vec<RankedHit>, String> {
    if cands.is_empty() {
        return Ok(Vec::new());
    }
    let refs: Vec<&str> = cands.iter().map(|c| c.content.as_str()).collect();
    let hits = reranker
        .rerank(query, &refs)
        .map_err(|e| format!("cross-encoder rerank: {e}"))?;
    Ok(hits
        .into_iter()
        .take(limit)
        .map(|h| {
            let mut rh = cands[h.original_index].hit.clone();
            rh.score = Some(h.score as f64);
            rh
        })
        .collect())
}

/// ColBERT MaxSim rerank using the BGE-M3 ColBERT head. A single batched
/// `embed_colbert` call shares the forward pass across the query + all
/// candidates; each candidate is then scored by late interaction
/// ([`colbert_maxsim`]) and the set is reordered + truncated to `limit`.
pub fn colbert_rerank(
    embedder: &Embedder,
    query: &str,
    cands: &[Candidate],
    limit: usize,
) -> Result<Vec<RankedHit>, String> {
    if cands.is_empty() {
        return Ok(Vec::new());
    }
    let mut texts: Vec<&str> = Vec::with_capacity(cands.len() + 1);
    texts.push(query);
    texts.extend(cands.iter().map(|c| c.content.as_str()));

    let mats = embedder
        .embed_colbert(&texts)
        .map_err(|e| format!("embed_colbert: {e}"))?;
    let (query_tok, cand_mats) = mats.split_first().ok_or("empty colbert output")?;
    let query_tok = query_tok
        .as_ref()
        .ok_or("query produced no ColBERT tokens")?;

    let mut scored: Vec<(usize, f32)> = cand_mats
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let s = m
                .as_ref()
                .map(|doc| colbert_maxsim(query_tok, doc))
                .unwrap_or(f32::NEG_INFINITY); // a candidate with no tokens sorts last
            (i, s)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored
        .into_iter()
        .take(limit)
        .map(|(i, s)| {
            let mut rh = cands[i].hit.clone();
            rh.score = Some(s as f64);
            rh
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmcp::reranker::RerankHit;

    /// A mock reranker that returns a caller-specified order, so the reorder /
    /// truncate / index-mapping logic is testable without loading a model.
    struct MockReranker(Vec<(usize, f32)>);
    impl Reranker for MockReranker {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn rerank(&self, _q: &str, _c: &[&str]) -> anyhow::Result<Vec<RerankHit>> {
            Ok(self
                .0
                .iter()
                .map(|&(original_index, score)| RerankHit {
                    original_index,
                    score,
                })
                .collect())
        }
    }

    #[test]
    fn cross_encoder_reorders_truncates_and_maps_back() {
        let cands = vec![
            Candidate::new("a.rs", "alpha"),
            Candidate::new("b.rs", "beta"),
            Candidate::new("c.rs", "gamma"),
        ];
        // Reranker says: c (idx2) best, then a (idx0), then b (idx1).
        let mock = MockReranker(vec![(2, 0.9), (0, 0.5), (1, 0.1)]);
        let out = cross_encoder_rerank(&mock, "q", &cands, 2).expect("rerank");
        assert_eq!(out.len(), 2, "truncated to limit");
        assert_eq!(out[0].path, "c.rs");
        assert_eq!(out[1].path, "a.rs");
        assert!(
            (out[0].score.unwrap() - 0.9).abs() < 1e-6,
            "carries rerank score (f32→f64)"
        );
    }

    #[test]
    fn cross_encoder_empty_is_empty() {
        let mock = MockReranker(vec![]);
        assert!(
            cross_encoder_rerank(&mock, "q", &[], 10)
                .expect("ok")
                .is_empty()
        );
    }
}
