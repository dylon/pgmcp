//! JSON-based data tables: client-defined tables of observation rows stored as
//! JSONB, with an optional typed-column schema, descriptive aggregation, and
//! multi-format report rendering.
//!
//! This is the **DB-free domain layer**: the closed vocabularies
//! ([`ColumnType`], [`FilterOp`], [`SortDir`], [`Combinator`], [`AggFunc`]), the
//! row-validation rules ([`validate`]), identifier safety ([`identifier`]), and
//! the report view-model + renderers ([`report`]). Everything here is pure and
//! unit-testable; no `sqlx`, no I/O.
//!
//! The SQL lives in [`crate::db::queries::data_tables`] and the MCP tools in
//! `crate::mcp::tools::data_tables`. Full design + rationale (notably the
//! *no-dynamic-DDL* safety posture — user "tables" are rows in three fixed
//! tables, never `CREATE TABLE` at tool-call time):
//! `docs/decisions/010-json-data-tables.md`.

pub mod aggregate;
pub mod column_type;
pub mod filter;
pub mod identifier;
pub mod link_target;
pub mod report;
pub mod validate;

pub use column_type::ColumnType;
pub use filter::{Combinator, FilterOp, SortDir};
pub use identifier::validate_identifier;
