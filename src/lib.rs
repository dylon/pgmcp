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

#![recursion_limit = "512"]

// BLAS provider for ndarray's `blas` feature (cblas-sys FFI) is wired by
// `build.rs`, which emits `cargo:rustc-link-lib=dylib=blis-mt` so the
// linker pulls AOCL-BLIS (libblis-mt.so.5) into the lib and bin targets.
// No `extern crate blas_src;` — there is no stub-provider crate involved.

pub mod api;
pub mod cli;
pub mod config;
pub mod context;
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
pub mod parsing;
pub mod reactive;
pub mod shutdown;
pub mod stats;
#[allow(dead_code)]
pub mod topic_store;
pub mod work_pool;
