//! Library surface for pgmcp. Exposes the crate's modules so that cargo
//! examples (`examples/*.rs`) and integration tests (`tests/*.rs`) can
//! depend on internal APIs such as `cron::topic_clustering::fuzzy_c_means`.
//!
//! The binary entry point lives in `src/main.rs` and keeps its own `mod`
//! declarations for the same source files — Rust compiles them once per
//! target (lib + bin), but there is no runtime duplication.
//!
//! Module layout mirrors `src/main.rs`; if you add a new top-level module
//! there, add it here too so external test/example surface stays aligned.

#![recursion_limit = "256"]

// Force linkage to BLAS (Intel MKL) for ndarray's blas feature. Mirrors
// the same `extern crate` pair in `src/main.rs` — the linker needs these
// on both the lib target and the bin target.
extern crate blas_src;
extern crate intel_mkl_src;

pub mod api;
pub mod config;
pub mod cron;
pub mod daemon;
pub mod daemon_state;
pub mod db;
pub mod embed;
pub mod error;
pub mod fcm;
pub mod graph;
pub mod indexer;
pub mod logging;
pub mod mcp;
#[allow(dead_code)]
pub mod mmap_array;
pub mod reactive;
pub mod shutdown;
pub mod stats;
#[allow(dead_code)]
pub mod topic_store;
pub mod work_pool;
