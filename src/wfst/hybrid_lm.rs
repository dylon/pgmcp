//! Per-project HybridLanguageModel wrapper.
//!
//! Wraps `libgrammstein::hybrid::HybridLanguageModel` (Modified
//! Kneser-Ney n-gram + subword embeddings + 4 interpolation
//! strategies) behind pgmcp's config surface. The model is the
//! rescoring engine for the third RRF leg of `tool_hybrid_search`:
//! the user query is tokenized → Damerau-Levenshtein candidates →
//! lattice → composed with this LM → Viterbi-decoded rewritten
//! query → fed back into `semantic_search` / `text_search` for the
//! third RRF stream.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 9.

use libgrammstein::hybrid::{HybridConfig, InterpolationStrategy};

/// pgmcp-side config knob for the n-gram-LM third leg of hybrid_search.
/// Maps directly to libgrammstein's `HybridConfig`; carried separately
/// so the cron and the tool can read it without taking a libgrammstein
/// dep through the config crate.
#[derive(Debug, Clone)]
pub struct HybridLmConfig {
    /// N-gram order (1-5). Default 3 (trigram).
    pub order: usize,
    /// Interpolation strategy between n-gram and subword embedding
    /// scores. Default `Linear { alpha: 0.8 }` — n-gram dominates.
    pub strategy: InterpolationStrategy,
    /// Score-cache size in entries. Default 50,000.
    pub cache_size: usize,
    /// Embedding smoothing constant. Default 1e-8.
    pub embedding_smoothing: f64,
    /// Softmax temperature. Default 1.0.
    pub temperature: f64,
}

impl Default for HybridLmConfig {
    fn default() -> Self {
        Self {
            order: 3,
            strategy: InterpolationStrategy::Linear { alpha: 0.8 },
            cache_size: 50_000,
            embedding_smoothing: 1e-8,
            temperature: 1.0,
        }
    }
}

impl HybridLmConfig {
    /// Convert into a libgrammstein `HybridConfig` for handing to
    /// `HybridLanguageModel::new`.
    pub fn to_grammstein(&self) -> HybridConfig {
        HybridConfig {
            strategy: self.strategy,
            cache_size: self.cache_size,
            embedding_smoothing: self.embedding_smoothing,
            temperature: self.temperature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_documented_values() {
        let cfg = HybridLmConfig::default();
        assert_eq!(cfg.order, 3);
        assert_eq!(cfg.cache_size, 50_000);
        assert_eq!(cfg.embedding_smoothing, 1e-8);
        assert_eq!(cfg.temperature, 1.0);
        // Default interpolation: linear with alpha 0.8.
        match cfg.strategy {
            InterpolationStrategy::Linear { alpha } => assert_eq!(alpha, 0.8),
            _ => panic!("default strategy should be Linear {{ alpha: 0.8 }}"),
        }
    }

    #[test]
    fn to_grammstein_preserves_fields() {
        let cfg = HybridLmConfig::default();
        let g = cfg.to_grammstein();
        assert_eq!(g.cache_size, cfg.cache_size);
        assert_eq!(g.embedding_smoothing, cfg.embedding_smoothing);
        assert_eq!(g.temperature, cfg.temperature);
    }
}
