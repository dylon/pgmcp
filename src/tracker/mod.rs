//! Work-item / plan tracker domain logic (DB-free, unit-testable).
//!
//! Realizes `~/.claude/plans/plan-mcp-support-for-moonlit-dongarra.md`. The
//! pure, side-effect-free core lives here so the trust-critical pieces
//! (transition legality, completion roll-up, validation) are testable without
//! a database; the SQL layer is `crate::db::queries` and the MCP tools are
//! `crate::mcp::tools::work_items`.
//!
//! - [`kind`] — the closed `WorkItemKind` taxonomy (single source of truth for
//!   the `work_items.kind` CHECK vocabulary).
//! - [`status`] — the closed `WorkItemStatus` lifecycle vocabulary.
//! - [`transition`] — the legal-transition matrix + actor-capability gate that
//!   makes "an agent self-verifies / self-defers" structurally impossible.
//!
//! Later phases add `rollup` (completion CTE helpers), `ingest` (plan→tree
//! parser), `validate` (definition rule checkers), and `definition` (TOML
//! (de)serialization) to this module.

pub mod ingest;
pub mod kind;
pub mod rollup;
pub mod status;
pub mod transition;
pub mod validate;
