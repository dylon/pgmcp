//! Test fixtures, mocks, and factories for `pgmcp`.
//!
//! This crate is **dev-only**. The parent `pgmcp` crate does not consume
//! it directly — the dependency edge is one-way (`pgmcp-testing → pgmcp`)
//! so Cargo doesn't have to compile `pgmcp` twice. Cross-crate tests that
//! exercise `pgmcp`'s public API with `pgmcp-testing` fixtures live in
//! `pgmcp-testing/tests/`.
//!
//! ## Module layout
//!
//! - [`mocks`] — trait implementations whose state tests configure
//!   directly via typed public fields.
//! - [`fixtures`] — factory functions returning sensible defaults for
//!   `Config`, `SystemContext`, deterministic embeddings, etc.
//! - [`db_harness`] — [`TestTransaction`](db_harness::TestTransaction) and
//!   [`TestDatabase`](db_harness::TestDatabase) — real-Postgres harnesses
//!   (no Docker) that connect to the user's local install via
//!   `PGMCP_TEST_DATABASE_URL` or `~/.config/pgmcp/test-config.toml`.
//! - [`cli_harness`] — [`PgmcpProcess`](cli_harness::PgmcpProcess), a
//!   subprocess manager for protocol / CLI E2E tests.
//! - [`proptest_config`] — shared `proptest::test_runner::Config`
//!   presets (`standard`, `heavy`, `slow`).
//! - [`skip`] — macros that yield a harness or clean-skip on
//!   configuration absence. `require_test_txn!` / `require_test_db!` are
//!   re-exported at the crate root so tests can write `pgmcp_testing::…`.

pub mod cli_harness;
pub mod db_harness;
pub mod eval;
pub mod fixtures;
pub mod golden;
pub mod mocks;
pub mod pool_tool_helpers;
pub mod proptest_config;
pub mod skip;
