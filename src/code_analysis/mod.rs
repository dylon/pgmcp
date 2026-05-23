//! Higher-level code-analysis layers built on Shadow-ASR.
//!
//! pgmcp's `src/parsing/` already runs tree-sitter / `syn` across 12
//! backends and populates `file_symbols`, `symbol_parameters`,
//! `symbol_effects`, `type_tag_catalog`, `effect_catalog`, and
//! `symbol_references` (4-tier `resolution_kind` classification).
//! This module layers `libgrammstein`'s code-analysis algorithms on top
//! of those tables: Code Property Graph, paradigm detection, frequent
//! subtree mining, and the GNN semantic-issue scorer.
//!
//! Phases that populate this module:
//! - **Phase 6**: `tree_sitter` adapter (PG rows → libgrammstein `Ast`),
//!   `cpg`, `paradigm`, `subtree`, `gnn` submodules. No re-parse — every
//!   call hydrates from Shadow-ASR.
//! - **Phase 8**: the user-facing MCP tools (`paradigm_profile`,
//!   `code_property_graph`, `subtree_mining`, `gnn_semantic_issues`)
//!   that consume this module.
