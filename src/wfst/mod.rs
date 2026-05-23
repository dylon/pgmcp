//! WFST-backed query correction + LM rescoring for pgmcp.
//!
//! Built on `lling-llang` (WFST framework: semirings, lattice builder,
//! Viterbi/n-best/beam, lazy composition, GPU paths) and consumes
//! `libgrammstein::hybrid::HybridLanguageModel` (Modified Kneser-Ney
//! n-gram + subword embeddings) for the language-model rescoring layer.
//!
//! Phases that populate this module:
//! - **Phase 9**: `lattice`, `hybrid_lm`, `query_rescore`, `correction`
//!   submodules — third RRF leg for `tool_hybrid_search` and
//!   `tool_correct_query`.
//! - **Phase 10**: lattice constructions also wired into
//!   `liblevenshtein`'s `PhoneticPipelineBuilder` for the phonetic
//!   integration.
