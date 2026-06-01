//! Severity scoring for concurrency findings.

use crate::graph::lock_order::LockCycle;
use crate::tracker::severity::Severity;

/// Score a lock-order cycle into a tracker [`Severity`] + a numeric score in
/// `[0,1]`: `score = min_confidence · rw_factor · (0.5 + 0.5·public_api_reachable)`,
/// where `rw_factor` is `0.3` for an all-read (non-deadlocking) cycle else `1.0`.
pub fn cycle_severity(cycle: &LockCycle, public_api_reachable: bool) -> (Severity, f32) {
    let conf = cycle.min_confidence().clamp(0.0, 1.0);
    let rw = if cycle.is_all_read() { 0.3 } else { 1.0 };
    let api = if public_api_reachable { 1.0 } else { 0.0 };
    let score = (conf * rw * (0.5 + 0.5 * api)).clamp(0.0, 1.0);
    let sev = if score >= 0.75 {
        Severity::Critical
    } else if score >= 0.45 {
        Severity::High
    } else if score >= 0.20 {
        Severity::Medium
    } else {
        Severity::Low
    };
    (sev, score)
}
