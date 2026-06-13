//! Higher-level code-analysis layers built on Shadow-ASR.
//!
//! pgmcp's `src/parsing/` already runs tree-sitter / `syn` across 12
//! backends and populates `file_symbols`, `symbol_parameters`,
//! `symbol_effects`, `type_tag_catalog`, `effect_catalog`, and
//! `symbol_references` (4-tier `resolution_kind` classification). This
//! module layers additional code-analysis algorithms on top of those
//! tables: CK metrics, test/doc coverage, anomaly models (LOF,
//! isolation forest, defect model), taint dataflow (intra/interproc),
//! and vulnerability matching.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 6.
//!
//! NOTE (2026-06-13): the libgrammstein-backed `cpg`, `paradigm`, and
//! `subtree` modules were removed together with the caller-supplied MCP
//! tools that were their only consumers (`code_property_graph`,
//! `paradigm_profile`, `subtree_mining`, `gnn_semantic_issues`). They
//! operated on inline agent-supplied code with no index linkage.

pub mod ast_rules;
pub mod ck_metrics;
pub mod coverage;
pub mod defect_model;
pub mod findings;
pub mod isolation_forest;
pub mod language_detect;
pub mod lof;
pub mod reflexion;
pub mod taint_dataflow;
pub mod taint_interproc;
pub mod taint_spec;
pub mod vuln_match;
