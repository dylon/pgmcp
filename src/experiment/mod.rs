//! Scientific-experiment subsystem: the MCP server prescribes the protocol
//! and arbitrates the data; agents execute the work and submit raw samples.
//!
//! - [`protocol`] — kind-aware experiment-design prescription (sample size via
//!   power, warm-up, the recommended test, the reproducibility checklist).
//!
//! The statistical engine and the acceptance-criterion taxonomy live in
//! `crate::stats::{inference, acceptance}`; the schema in
//! `crate::db::migrations::ensure_experiment_tables`; the query layer in
//! `crate::db::queries`; the MCP tools in
//! `crate::mcp::tools::tool_experiments`. Later phases add the memory-graph
//! mirror, the ledger renderer, and the CLI runner here.
//!
//! Design: `docs/experiments/README.md` and
//! `~/.claude/plans/plan-how-to-effectively-drifting-fox.md`.

pub mod extract;
pub mod ledger;
pub mod mirror;
pub mod pinning;
pub mod protocol;
pub mod relation;
pub mod runner;
pub mod spec;
