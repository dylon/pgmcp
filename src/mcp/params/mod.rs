//! Tool parameter types for the MCP server.
//!
//! Every `#[tool]` handler deserializes its arguments into one of the
//! `*Params` structs defined here. The structs were extracted verbatim from
//! `server.rs` (B.2 god-file split) and split into per-domain files purely to
//! keep each file small; they are re-exported with glob `pub use` so that
//! `crate::mcp::server::<Name>Params` continues to resolve for the ~197 tool
//! body files in `src/mcp/tools/` and for the `dispatch_tool!` / CLI paths in
//! `server.rs`.
//!
//! Each child module does `use super::*;` so cross-file field references (e.g.
//! a `*Params` whose field is a helper input struct living in a sibling file)
//! resolve without explicit per-struct imports.
#![allow(unused_imports)]

pub mod a2a_csm;
pub mod core_memory_a;
pub mod data_tables;
pub mod graph_arch;
pub mod memory_sema;
pub mod recommend;
pub mod search;
pub mod sota_a;
pub mod work_items_a;
pub mod work_items_b;

pub use a2a_csm::*;
pub use core_memory_a::*;
pub use data_tables::*;
pub use graph_arch::*;
pub use memory_sema::*;
pub use recommend::*;
pub use search::*;
pub use sota_a::*;
pub use work_items_a::*;
pub use work_items_b::*;
