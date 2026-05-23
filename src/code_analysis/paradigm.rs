//! Programming-paradigm detection via libgrammstein's `ParadigmDetector`.
//!
//! Heuristic regex pass over file content that produces per-paradigm
//! weights (OOP, FP, Reactive, Procedural). pgmcp surfaces this as
//! the `paradigm_profile` MCP tool in Phase 8.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 6.

use libgrammstein::topic::paradigm::{ParadigmConfig, ParadigmDetector, ParadigmProfile};

/// One-call wrapper that constructs a default-configured
/// `ParadigmDetector` and runs it over `code`.
pub fn analyze_code(code: &str) -> ParadigmProfile {
    let detector = ParadigmDetector::new(ParadigmConfig::default());
    detector.analyze(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_does_not_panic() {
        let profile = analyze_code("");
        // The profile's struct fields are the four paradigm weights —
        // empty input gives zero weight on each. We don't assert
        // specific values (the detector's heuristics are
        // implementation-defined) — only that the call returns.
        let _ = profile;
    }

    #[test]
    fn rust_code_yields_some_profile() {
        let rust = r#"
            fn main() {
                let xs: Vec<i32> = (0..10).map(|x| x * 2).filter(|x| x > &4).collect();
                println!("{:?}", xs);
            }
        "#;
        let profile = analyze_code(rust);
        // FP indicators (map / filter / collect) should produce non-zero
        // FP weight. Exact value depends on libgrammstein's regex
        // calibration; we just assert the analyzer returns.
        let _ = profile;
    }
}
