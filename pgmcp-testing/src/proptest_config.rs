//! Shared [`proptest::test_runner::Config`] for the pgmcp test suite.
//!
//! Centralizing the knobs means adjusting shrink iterations, case counts,
//! or regression-file location once in one place instead of in every
//! `proptest!{ #![proptest_config(...)] }` block.
//!
//! Usage:
//!
//! ```ignore
//! use proptest::proptest;
//! proptest! {
//!     #![proptest_config(pgmcp_testing::proptest_config::standard())]
//!     #[test]
//!     fn prop_foo(x in 0..100u32) { /* ... */ }
//! }
//! ```
//!
//! Heavy proptests (FCM, graph algorithms) that run slow enough to dominate
//! `verify.sh` gate 5 should use [`heavy`] for the smaller 64-case budget.

use proptest::test_runner::Config;

/// Default configuration — 256 cases per property. Good signal/cost tradeoff
/// for fast pure-function tests.
pub fn standard() -> Config {
    Config {
        cases: 256,
        max_shrink_iters: 4096,
        ..Config::default()
    }
}

/// Reduced configuration for heavy properties (graph algorithms, FCM math,
/// anything involving large matrices). 64 cases still surfaces real bugs
/// without bloating CI wall-clock time.
pub fn heavy() -> Config {
    Config {
        cases: 64,
        max_shrink_iters: 2048,
        ..Config::default()
    }
}

/// Tight configuration for slow property checks where each case runs in
/// the hundreds-of-ms range (e.g. spawning tasks, I/O-ish operations).
pub fn slow() -> Config {
    Config {
        cases: 16,
        max_shrink_iters: 512,
        ..Config::default()
    }
}
