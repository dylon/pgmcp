#[allow(dead_code)]
pub mod client;
pub mod migrations;
#[allow(dead_code)]
pub mod pool;
#[allow(dead_code)]
pub mod queries;

#[allow(unused_imports)] // Re-exported for callers in Phase 3+; not yet wired internally.
pub use client::DbClient;
