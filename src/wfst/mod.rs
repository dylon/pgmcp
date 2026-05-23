//! WFST-backed query correction + LM rescoring for pgmcp.
//!
//! Built on `lling-llang` (WFST framework: semirings, lattice builder,
//! Viterbi/n-best/beam, lazy composition, GPU paths) and consumes
//! `libgrammstein::hybrid::HybridLanguageModel` (Modified Kneser-Ney
//! n-gram + subword embeddings) for the language-model rescoring layer.
//!
//! Phases that populate this module:
//! - **Phase 9**: `hybrid_lm` submodule — per-project HybridLanguageModel
//!   wrapper backing the third RRF leg in `tool_hybrid_search`.
//! - **Phase 10**: lattice constructions also wired into
//!   `liblevenshtein`'s `PhoneticPipelineBuilder` for the phonetic
//!   integration.

pub mod hybrid_lm;

#[allow(unused_imports)]
pub use hybrid_lm::HybridLmConfig;
