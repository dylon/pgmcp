//! Higher-level code-analysis layers built on Shadow-ASR.
//!
//! pgmcp's `src/parsing/` already runs tree-sitter / `syn` across 12
//! backends and populates `file_symbols`, `symbol_parameters`,
//! `symbol_effects`, `type_tag_catalog`, `effect_catalog`, and
//! `symbol_references` (4-tier `resolution_kind` classification). This
//! module layers `libgrammstein`'s code-analysis algorithms on top of
//! those tables: Code Property Graph, paradigm detection, frequent
//! subtree mining, and (when the upstream `ort`/`ndarray` version skew
//! is resolved) the GNN semantic-issue scorer.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 6.

pub mod ast_rules;
pub mod cpg;
pub mod isolation_forest;
pub mod language_detect;
pub mod lof;
pub mod paradigm;
pub mod reflexion;
pub mod subtree;
pub mod taint_dataflow;
pub mod taint_interproc;
pub mod taint_spec;
