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

// `serde_json::json!` in `src/stats/tracker.rs` builds the stats snapshot as one
// ~250-field object literal; the macro recurses once per field, exceeding rustc's
// default macro-expansion depth (128). Lift it to 1024 (the long-standing value;
// generous headroom over the literal). This is NOT related to `AcceptanceCriterion`
// serde — that recursive enum uses adjacent tagging (ADR-006) and needs no bump.
#![recursion_limit = "1024"]

// BLAS provider for ndarray's `blas` feature (cblas-sys FFI) is wired by
// `build.rs`, which emits `cargo:rustc-link-lib=dylib=blis-mt` so the
// linker pulls AOCL-BLIS (libblis-mt.so.5) into the lib and bin targets.
// No `extern crate blas_src;` — there is no stub-provider crate involved.

pub mod a2a;
pub mod adoption;
pub mod api;
pub mod cli;
#[allow(dead_code)]
pub mod code_analysis;
pub mod config;
pub mod context;
pub mod cron;
pub mod csm;
pub mod daemon;
pub mod daemon_state;
pub mod db;
pub mod embed;
pub mod error;
pub mod experiment;
pub mod fcm;
#[allow(dead_code)]
pub mod fuzzy;
pub mod graph;
pub mod indexer;
pub mod llm;
pub mod logging;
pub mod mandates;
pub mod mcp;
#[allow(dead_code)]
pub mod mmap_array;
#[allow(dead_code)]
pub mod neural;
pub mod parsing;
pub mod patterns;
pub mod quality;
pub mod reactive;
pub mod render;
pub mod reranker;
pub mod rmas;
pub mod sessions;
pub mod shutdown;
pub mod stats;
#[allow(dead_code)]
pub mod topic_store;
pub mod tracker;
#[allow(dead_code)]
pub mod wfst;
pub mod work_pool;
