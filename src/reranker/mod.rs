//! Memory-server Phase 7: cross-encoder reranker.
//!
//! Pluggable backend for query-vs-candidate scoring on top of a
//! pre-fetched candidate list. Follows the `FcmBackend` /
//! `LlmExtractor` patterns: trait + closed-set enum + factory.
//!
//! Production backend: `BgeRerankerV2M3` — BGE-reranker-v2-m3 via
//! candle, XLM-RoBERTa architecture with a sigmoided relevance head.
//! Sized for the user's RTX 4060 Ti (~600 MB fp16). The reranker is
//! mutually exclusive in VRAM with the Qwen3 extractor (per the
//! Phase-11 hardware budget) — the dispatcher unloads one to make room
//! for the other.

#![allow(dead_code)]

use anyhow::Result;

pub mod bge_v2_m3;

/// Reranked candidate.
#[derive(Debug, Clone, Copy)]
pub struct RerankHit {
    pub original_index: usize,
    pub score: f32,
}

/// Cross-encoder scorer. `rerank` is synchronous (candle forward is
/// sync); the caller wraps in `spawn_blocking` if it doesn't want to
/// block the runtime.
pub trait Reranker: Send + Sync {
    fn name(&self) -> &'static str;
    /// Returns the candidates re-sorted descending by score (highest
    /// first), tagged with their original index for round-tripping
    /// back to the original list.
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<RerankHit>>;
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RerankerChoice {
    BgeV2M3,
    Disabled,
}

pub fn parse_reranker_choice(s: &str) -> Result<RerankerChoice> {
    match s {
        "bge-v2-m3" | "bge-reranker-v2-m3" => Ok(RerankerChoice::BgeV2M3),
        "disabled" | "off" | "none" => Ok(RerankerChoice::Disabled),
        other => Err(anyhow::anyhow!(
            "unknown reranker backend '{}'; choices: bge-v2-m3, disabled",
            other
        )),
    }
}

pub fn make_reranker(choice: RerankerChoice) -> Result<Option<Box<dyn Reranker>>> {
    match choice {
        RerankerChoice::Disabled => Ok(None),
        RerankerChoice::BgeV2M3 => Ok(Some(Box::new(bge_v2_m3::BgeRerankerV2M3::new()?))),
    }
}

/// Cheap rerank helper: pairs the query with each candidate, applies
/// the reranker, and returns the top-K candidates as `(original_index,
/// score)` pairs.
pub fn rerank_topk(
    reranker: &dyn Reranker,
    query: &str,
    candidates: &[&str],
    top_k: usize,
) -> Result<Vec<RerankHit>> {
    let mut hits = reranker.rerank(query, candidates)?;
    if hits.len() > top_k {
        hits.truncate(top_k);
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reranker_choice_round_trip() {
        assert!(matches!(
            parse_reranker_choice("bge-v2-m3").unwrap(),
            RerankerChoice::BgeV2M3
        ));
        assert!(matches!(
            parse_reranker_choice("disabled").unwrap(),
            RerankerChoice::Disabled
        ));
        assert!(parse_reranker_choice("nonsense").is_err());
    }
}
