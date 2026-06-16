//! Topic-model **portfolio analytics**: read-only analyses that turn the stored
//! topic model (`code_topics` centroids/keywords, `chunk_topic_assignments` soft
//! memberships, per-scope `topics_quality`) into project-level insight —
//! profiling, cross-project overlap, fork/redundancy detection, longitudinal
//! trends, per-topic ownership, concern coupling, and coverage gaps.
//!
//! Follows the `src/quality` aggregator convention: **independent collectors
//! query tables directly** (via [`loaders`]) and compose **pure measures**
//! ([`measures`]); the existing topic tools are left untouched. Each collector
//! has a thin MCP handler in `src/mcp/tools/tool_<name>.rs` and renders via
//! [`render`] in all six report formats.
//!
//! `#![allow(dead_code)]` mirrors `src/quality/mod.rs`: helpers are shared across
//! collectors and not every one is reached from the binary target.
#![allow(dead_code)]

pub mod loaders;
pub mod measures;
pub mod render;

// Theme ① — profiling & forks
pub mod profile;
pub mod project_map;
pub mod similarity;

// Theme ② — trends & forecast
pub mod trends;

// Theme ③ — ownership
pub mod owners;

// Theme ④ — concern coupling & gaps
pub mod cooccurrence;
pub mod gaps;
