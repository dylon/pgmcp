//! Quality reporting — aggregate the findings produced by pgmcp's ~33 analysis
//! tools into a single graded, three-pillar (Engineering / Architecture /
//! Security) report rendered by `crate::render`.
//!
//! - [`findings`] — the canonical [`Finding`] / [`Severity`] / [`FindingCategory`]
//!   / [`Pillar`] vocabulary every `collect_*` helper emits.
//! - [`report`] — the graded [`QualityReport`] tree + grade arithmetic.
//! - [`aggregate`] — fans the analysis tools out and assembles a report.
//! - [`history`] — persists per-pillar GPAs for trend rendering.
//! - [`forecast`] — pure trend/slope/threshold math over a metric series
//!   (snapshots → trajectories), reused by the trend tools and the digest.
//!
//! Much of this surface is exercised only through the `quality_report` MCP tool
//! (registered via the `#[tool_router]` macro), which defeats the compiler's
//! dead-code analysis; the module-level allow mirrors `code_analysis` / `fuzzy`.
#![allow(dead_code)]

pub mod aggregate;
pub mod collectors;
pub mod findings;
pub mod forecast;
pub mod history;
pub mod report;
pub mod retrieval_drift;
pub mod retrieval_metrics;
pub mod topic_metrics;

pub use findings::{Finding, FindingCategory, Pillar, Severity};
pub use report::{PillarReport, QualityReport};
