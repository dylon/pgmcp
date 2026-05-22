//! Language-agnostic complexity scoring.
//!
//! Each `LanguageBackend` walks its AST, populates a `ScoringInput` (defined
//! in `function_metrics.rs`), and calls `score()`. The math here is shared
//! across all languages — Halstead vocabularies, cognitive deltas, and the
//! Maintainability Index formula are universal once the per-language token
//! classification has happened upstream.

#![allow(dead_code)] // Functions are called by `LanguageBackend` impls and
// the function-metrics cron — backends land incrementally, so the
// foundational scoring layer is allowed to compile clean ahead of consumers.

use super::function_metrics::{
    CognitiveKind, FunctionMetrics, HalsteadCounts, NPathValue, ScoringInput,
};

/// Compute cyclomatic complexity from a decision-point count: `CC = 1 + dp`.
pub fn cyclomatic_complexity(decision_points: u32) -> u32 {
    1 + decision_points
}

/// Sum cognitive-complexity increments. Each `NestedCondition` contributes
/// `1 + depth`; `BreakInFlow`, `LogicalSequence`, and `Recursion` contribute
/// `1` each (Campbell, SonarSource 2018).
pub fn cognitive_complexity(input: &ScoringInput<'_>) -> u32 {
    let mut total: u32 = 0;
    for inc in &input.cognitive_increments {
        let delta = match inc.kind {
            CognitiveKind::NestedCondition => 1u32 + inc.depth as u32,
            CognitiveKind::BreakInFlow
            | CognitiveKind::LogicalSequence
            | CognitiveKind::Recursion => 1,
        };
        total = total.saturating_add(delta);
    }
    total
}

/// Multiply NPath factors with overflow checking. Each factor is clamped to
/// at least 1 (a decision point always contributes ≥1 paths).
pub fn npath_product(factors: &[u64]) -> NPathValue {
    let mut prod: u64 = 1;
    for &f in factors {
        let f = f.max(1);
        match prod.checked_mul(f) {
            Some(p) => prod = p,
            None => return NPathValue::Overflowed,
        }
    }
    NPathValue::Counted(prod)
}

/// Maintainability Index (SEI variant of Oman & Hagemeister 1992),
/// clamped to `[0, 100]`:
///
/// `MI = 171 - 5.2·ln(V) - 0.23·CC - 16.2·ln(LOC) + 50·sin(√(2.4·CR))`
///
/// where V is Halstead volume, CC is cyclomatic complexity, LOC is
/// source-line count, and CR = comment_lines / max(1, LOC).
///
/// Inputs are defensively floored so `ln(0)` and `NaN`/`±Infinity`
/// propagations cannot escape the clamp.
pub fn maintainability_index(volume: f64, cc: u32, loc: u32, comment_lines: u32) -> f64 {
    let v = volume.max(1.0);
    let loc_f = (loc.max(1)) as f64;
    let cr = if loc == 0 {
        0.0
    } else {
        (comment_lines as f64) / (loc as f64)
    };
    let mi = 171.0 - 5.2 * v.ln() - 0.23 * cc as f64 - 16.2 * loc_f.ln()
        + 50.0 * (2.4 * cr).max(0.0).sqrt().sin();
    if mi.is_nan() {
        // Defensive: should not happen given the floors, but guarantee a
        // sane value rather than poisoning downstream aggregates.
        return 0.0;
    }
    mi.clamp(0.0, 100.0)
}

/// Aggregate a `ScoringInput` into a fully-realized `FunctionMetrics` row.
/// The output's `function_id` and `file_id` are placeholders (`0`); the
/// cron resolves them via `file_symbols` lookup before persisting.
pub fn score(input: &ScoringInput<'_>) -> FunctionMetrics {
    let cyclomatic = cyclomatic_complexity(input.decision_points);
    let cognitive = cognitive_complexity(input);
    let halstead = halstead_from_input(input);
    let npath = npath_product(&input.npath_factors);
    FunctionMetrics {
        function_id: 0,
        file_id: 0,
        name: input.name.to_string(),
        start_line: input.start_line,
        end_line: input.end_line,
        cyclomatic,
        cognitive,
        halstead,
        npath,
        loc: input.source_lines,
        comment_lines: input.comment_lines,
        panic_paths: input.panic_paths,
        unsafe_blocks: input.unsafe_blocks,
    }
}

impl FunctionMetrics {
    /// Compute Maintainability Index for this metrics row (used by the cron
    /// when assembling the UNNEST batch column). Centralized so the formula
    /// lives in `complexity.rs` only.
    pub fn maintainability_index(&self) -> f64 {
        maintainability_index(
            self.halstead.volume(),
            self.cyclomatic,
            self.loc,
            self.comment_lines,
        )
    }
}

fn halstead_from_input(input: &ScoringInput<'_>) -> HalsteadCounts {
    let n1 = input.operators.len() as u32;
    let n2 = input.operands.len() as u32;
    let big_n1: u32 = input.operators.values().copied().sum();
    let big_n2: u32 = input.operands.values().copied().sum();
    HalsteadCounts {
        n1,
        n2,
        big_n1,
        big_n2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::function_metrics::{CognitiveIncrement, CognitiveKind};

    #[test]
    fn cyclomatic_empty_function_is_one() {
        assert_eq!(cyclomatic_complexity(0), 1);
    }

    #[test]
    fn cyclomatic_single_if_is_two() {
        assert_eq!(cyclomatic_complexity(1), 2);
    }

    #[test]
    fn cyclomatic_match_with_n_arms_counts_each_arm() {
        // CC = 1 + n_arms when each arm is its own decision point.
        assert_eq!(cyclomatic_complexity(4), 5);
    }

    #[test]
    fn cognitive_break_in_flow_counts_one() {
        let input = ScoringInput {
            name: "f",
            cognitive_increments: vec![CognitiveIncrement {
                depth: 0,
                kind: CognitiveKind::BreakInFlow,
            }],
            ..ScoringInput::default()
        };
        assert_eq!(cognitive_complexity(&input), 1);
    }

    #[test]
    fn cognitive_nested_condition_uses_depth() {
        let input = ScoringInput {
            name: "f",
            cognitive_increments: vec![
                CognitiveIncrement {
                    depth: 0,
                    kind: CognitiveKind::NestedCondition,
                }, // +1
                CognitiveIncrement {
                    depth: 1,
                    kind: CognitiveKind::NestedCondition,
                }, // +2
                CognitiveIncrement {
                    depth: 2,
                    kind: CognitiveKind::NestedCondition,
                }, // +3
            ],
            ..ScoringInput::default()
        };
        assert_eq!(cognitive_complexity(&input), 6);
    }

    #[test]
    fn cognitive_logical_sequence_counts_one_per_change() {
        // `a && b || c` switches operator twice in the chain.
        let input = ScoringInput {
            name: "f",
            cognitive_increments: vec![
                CognitiveIncrement {
                    depth: 0,
                    kind: CognitiveKind::LogicalSequence,
                },
                CognitiveIncrement {
                    depth: 0,
                    kind: CognitiveKind::LogicalSequence,
                },
            ],
            ..ScoringInput::default()
        };
        assert_eq!(cognitive_complexity(&input), 2);
    }

    #[test]
    fn cognitive_recursion_counts_one() {
        let input = ScoringInput {
            name: "f",
            cognitive_increments: vec![CognitiveIncrement {
                depth: 5,
                kind: CognitiveKind::Recursion,
            }],
            ..ScoringInput::default()
        };
        // Recursion is +1 regardless of depth.
        assert_eq!(cognitive_complexity(&input), 1);
    }

    #[test]
    fn npath_empty_factor_list_is_one() {
        let n = npath_product(&[]);
        assert!(matches!(n, NPathValue::Counted(1)));
    }

    #[test]
    fn npath_single_zero_factor_clamped_to_one() {
        let n = npath_product(&[0]);
        assert!(matches!(n, NPathValue::Counted(1)));
    }

    #[test]
    fn npath_product_multiplies_factors() {
        let n = npath_product(&[2, 3, 4]);
        assert!(matches!(n, NPathValue::Counted(24)));
    }

    #[test]
    fn npath_overflow_returns_overflowed() {
        let n = npath_product(&[u64::MAX, 2]);
        assert!(matches!(n, NPathValue::Overflowed));
    }

    #[test]
    fn mi_clamped_to_zero_when_volume_huge() {
        let mi = maintainability_index(1.0e300, 1000, 100_000, 0);
        assert!((0.0..=100.0).contains(&mi));
    }

    #[test]
    fn mi_clamped_to_hundred_when_trivial() {
        // V=1, CC=1, LOC=1, no comments → MI well above 100, must clamp.
        let mi = maintainability_index(1.0, 1, 1, 0);
        assert_eq!(mi, 100.0);
    }

    #[test]
    fn mi_handles_nan_inputs() {
        let mi = maintainability_index(f64::NAN, 0, 0, 0);
        assert!((0.0..=100.0).contains(&mi));
    }

    #[test]
    fn mi_handles_infinity_inputs() {
        let mi = maintainability_index(f64::INFINITY, 0, 0, 0);
        assert!((0.0..=100.0).contains(&mi));
    }

    #[test]
    fn mi_decreases_with_complexity() {
        // Use parameters large enough that the result doesn't clamp to 100
        // on the easy side. V=1e6 + LOC=1000 places the unclamped MI well
        // below 100, so the CC delta is observable.
        let easy = maintainability_index(1.0e6, 1, 1000, 100);
        let hard = maintainability_index(1.0e6, 200, 1000, 100);
        assert!(
            hard < easy,
            "MI should fall as CC rises (easy={}, hard={})",
            easy,
            hard
        );
    }

    #[test]
    fn score_assembles_metrics_correctly() {
        use std::collections::HashMap;
        let mut operators = HashMap::new();
        operators.insert("+", 3u32);
        operators.insert("-", 2);
        let mut operands = HashMap::new();
        operands.insert("a".to_string(), 4u32);
        operands.insert("b".to_string(), 3);
        let input = ScoringInput {
            name: "compute",
            start_line: 10,
            end_line: 30,
            decision_points: 3,
            operators,
            operands,
            npath_factors: vec![2, 3],
            source_lines: 20,
            comment_lines: 4,
            ..ScoringInput::default()
        };
        let m = score(&input);
        assert_eq!(m.name, "compute");
        assert_eq!(m.start_line, 10);
        assert_eq!(m.end_line, 30);
        assert_eq!(m.cyclomatic, 4); // 1 + 3
        assert_eq!(m.cognitive, 0); // no increments
        assert_eq!(m.halstead.n1, 2);
        assert_eq!(m.halstead.n2, 2);
        assert_eq!(m.halstead.big_n1, 5);
        assert_eq!(m.halstead.big_n2, 7);
        assert!(matches!(m.npath, NPathValue::Counted(6)));
        assert_eq!(m.loc, 20);
        assert_eq!(m.comment_lines, 4);
    }

    use proptest::prelude::*;

    proptest! {
        /// MI is always in [0, 100] regardless of input.
        #[test]
        fn prop_mi_always_in_zero_hundred(
            v in 0.0_f64..1.0e6,
            cc in 0u32..10000,
            loc in 0u32..1_000_000,
            cl in 0u32..1_000_000,
        ) {
            let mi = maintainability_index(v, cc, loc, cl);
            prop_assert!((0.0..=100.0).contains(&mi));
        }

        /// Cyclomatic + 0 decision points is always 1.
        #[test]
        fn prop_cyclomatic_floor_is_one(dp in 0u32..10000) {
            prop_assert_eq!(cyclomatic_complexity(dp), 1 + dp);
        }

        /// NPath product is monotonic in any positive factor.
        #[test]
        fn prop_npath_monotonic(a in 1u64..1_000_000, b in 1u64..1_000_000) {
            let single = npath_product(&[a]);
            let both = npath_product(&[a, b]);
            if let (NPathValue::Counted(x), NPathValue::Counted(y)) = (single, both) {
                prop_assert!(y >= x);
            }
        }
    }
}
