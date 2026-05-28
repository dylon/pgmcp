//! Independent, finding-focused collectors over the indexed tables.
//!
//! Each `collect_<name>` queries the same underlying tables a corresponding MCP
//! analysis tool reads, but returns the unified [`Finding`] vocabulary directly
//! — uncapped and typed (no JSON round-trip, no stats-counter side effects, no
//! refactor of the working tools). The aggregator fans these out and the
//! pillars/findings are assembled from their output.
//!
//! Severity is synthesized per-collector following the calibration table in the
//! plan: tools that already tier their output pass it through; the rest map raw
//! scores to fixed cutoffs (not quantiles, which would flap between runs).
#![allow(dead_code)]

pub mod architecture;
pub mod code_health;
pub mod concurrency;
pub mod dependency;
pub mod duplication;
pub mod hygiene;
pub mod security;
pub mod tests_docs;

/// Marker regex for documented-tech-debt scans (shared by a couple collectors).
pub(crate) const DEBT_MARKER_PATTERN: &str = r"(?i)\b(TODO|FIXME|HACK|XXX|TEMP|WORKAROUND|BUG)\b";

/// Truncate a string to `max` chars with an ellipsis, for finding previews.
pub(crate) fn truncate_preview(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
