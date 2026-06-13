pub mod admin;
#[allow(dead_code)]
pub mod client;
pub mod disk_read;
pub mod mcp_tool_catalog;
pub mod migrations;
pub mod ontology;
pub mod patterns;
#[allow(dead_code)]
pub mod pool;
#[allow(dead_code)]
pub mod queries;
pub mod tool_cards;

#[allow(unused_imports)] // Re-exported for callers in Phase 3+; not yet wired internally.
pub use client::DbClient;
