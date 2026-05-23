//! Shared helpers for shadow-ASR-aware MCP tools.
//!
//! This module is the single home for the JOIN patterns, type-shape
//! computations, and resolved-edge traversals that the upgraded tools
//! (Phase D2b in the unified-semantic-representation plan) need. Putting
//! them here keeps per-tool diffs to a single helper call plus
//! tool-specific assembly, rather than ~120 tools each reimplementing
//! the same SQL.
//!
//! Submodules:
//! - [`signatures`]: structured signature descriptors
//!   (`SignatureDescriptor`, `signature_shape_hash`, `signature_diff`).
//! - [`effects`]: queries over `symbol_effects` (symbols with effect,
//!   effect set for symbol, effect reachability).
//! - [`edges`]: resolved call-edge traversals using `target_path` /
//!   `resolution_kind` / `resolution_confidence`.
//! - [`filters`]: request-parameter parsing for shadow-ASR-aware filter
//!   arguments (`type_tags`, `effects`, `min_confidence`).
//! - [`equivalence`]: cross-language signature equivalence reads from
//!   the `cross_language_signature_clones` materialized table.
//! - [`chunk_symbol_overlay`]: chunk ↔ symbol overlay for topic/chunk
//!   tools (Pattern G).
//! - [`a2a_capabilities`]: typed capability descriptors for A2A agents
//!   (Pattern H).
//! - [`memory_anchor`]: symbol facets stored on memory↔code relations
//!   (Pattern I).
//!
//! Every helper is `pub(crate)` because tools across `src/mcp/tools/`
//! consume them and rust-analyzer's dead-code analysis would otherwise
//! flag them while individual tool upgrades roll in incrementally.

#![allow(dead_code)]

pub mod a2a_capabilities;
pub mod chunk_symbol_overlay;
pub mod edges;
pub mod effects;
pub mod equivalence;
pub mod filters;
pub mod memory_anchor;
pub mod signatures;
