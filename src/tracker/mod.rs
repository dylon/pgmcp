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
//! - [`severity`] — the closed `Severity` (bug impact) and `BugResolution`
//!   vocabularies, orthogonal to the `priority` (urgency) axis.
//! - [`transition`] — the legal-transition matrix + actor-capability gate that
//!   makes "an agent self-verifies / self-defers" structurally impossible.
//! - [`git_link`] — the closed `GitLinkType` / `FindingSource` vocabularies for
//!   the Phase-3 git/PR close-the-loop layer.
//! - [`commit_ref`] — pure parsing of the `#<public_id>` / `fixes <public_id>`
//!   commit-message convention (shared by the indexer + REST).
//! - [`auto_transition`] — the pure agent-grade policy mapping a commit/PR
//!   reference to the status to advance to; structurally incapable of reaching
//!   a judgment status (the Phase-3 trust pin).
//!
//! Later phases add `rollup` (completion CTE helpers), `ingest` (plan→tree
//! parser), `validate` (definition rule checkers), and `definition` (TOML
//! (de)serialization) to this module.

pub mod auto_transition;
pub mod commit_ref;
pub mod git_link;
pub mod ingest;
pub mod kind;
pub mod rollup;
pub mod severity;
pub mod status;
pub mod transition;
pub mod validate;
pub mod views;
