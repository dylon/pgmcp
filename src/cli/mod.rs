//! CLI subcommand handlers — one module per `Commands` variant.
//!
//! Each module exposes a `pub async fn run(...)` (or one of the named
//! entry points used by `admin`) that owns the full body of the matching
//! match arm in `main.rs`. `main.rs` itself stays thin — it parses args
//! and dispatches.
//!
//! Tests for any helper that previously lived as a private function in
//! `main.rs` (notably `tool::parse_tool_args`) become externally visible
//! once they live here.

pub mod a2a_adapter;
pub mod admin;
pub mod analyze;
pub mod context;
pub mod daemon;
pub mod experiment;
pub mod import_advisories;
pub mod ledger;
pub mod reindex;
pub mod results;
pub mod statistics;
pub mod status;
pub mod tool;
