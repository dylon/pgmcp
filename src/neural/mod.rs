//! Neural model wrappers for pgmcp.
//!
//! Built on candle (pure-Rust BERT inference with CUDA) and
//! `libgrammstein`'s `ModernBertEmbedder` / `ModernBertRescorer` /
//! `Summarizer`. pgmcp's existing `src/embed/` continues to own the
//! current BGE-M3 path; this module adds the ModernBERT loader
//! alongside it in Phase 5.
//!
//! Phases that populate this module:
//! - **Phase 5**: `modernbert` submodule — wraps
//!   `libgrammstein::neural::ModernBertEmbedder` for pgmcp's indexer
//!   pool. Coordinates with the existing `src/embed/model.rs` rather
//!   than replacing it.
//! - **Phase 5/8**: `rescorer` + `summarizer` submodules — backing for
//!   downstream MCP tools that use neural rescoring or extractive
//!   summarization with MMR diversity.
