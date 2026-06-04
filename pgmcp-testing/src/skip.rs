//! `require_test_txn!` / `require_test_db!` — test guards that yield a
//! harness handle or cleanly skip the test with a visible "SKIPPED" line.
//!
//! Use in every test that depends on a real PostgreSQL instance:
//!
//! ```ignore
//! #[tokio::test]
//! async fn my_sql_test() {
//!     let mut txn = pgmcp_testing::require_test_txn!();
//!     // …use txn.conn()
//! }
//! ```
//!
//! When no test database authority is available (`PGMCP_TEST_DATABASE_URL`,
//! `~/.config/pgmcp/test-config.toml`, or `~/.config/pgmcp/config.toml`), the
//! macro prints `SKIPPED: ...` to stderr and `return`s from the enclosing
//! function. This lets `./scripts/verify.sh` stay green for contributors who
//! don't have a local Postgres+pgvector install, while turning real-DB gating on
//! automatically when local pgmcp database config is present.

/// Begin a [`TestTransaction`](crate::db_harness::TestTransaction). On
/// failure, prints a human-readable "SKIPPED" line and returns from the
/// calling function.
#[macro_export]
macro_rules! require_test_txn {
    () => {
        match $crate::db_harness::TestTransaction::begin().await {
            Ok(txn) => txn,
            Err(e) => {
                eprintln!("SKIPPED: {}", e);
                return;
            }
        }
    };
}

/// Create a fresh [`TestDatabase`](crate::db_harness::TestDatabase). On
/// failure, prints a human-readable "SKIPPED" line and returns from the
/// calling function.
#[macro_export]
macro_rules! require_test_db {
    () => {
        match $crate::db_harness::TestDatabase::new().await {
            Ok(db) => db,
            Err(e) => {
                eprintln!("SKIPPED: {}", e);
                return;
            }
        }
    };
}
