//! Per-function complexity metrics.
//!
//! Mirrors the `function_metrics` table in `src/db/migrations.rs`. Backends
//! return rows with placeholder `function_id = 0` / `file_id = 0`; the cron
//! (`src/cron/function_metrics.rs`) looks up `file_symbols.id` by
//! `(file_id, kind='function', name, start_line)` before bulk-inserting.
//!
//! Scoring is split into a per-language tree-sitter / syn pass (which fills
//! `ScoringInput` in `src/parsing/complexity.rs`) and a language-agnostic
//! formula evaluator. Halstead operator/operand vocabularies are
//! per-language; everything else is shared.

#![allow(dead_code)] // Types are wired up by `LanguageBackend` impls and the
// function-metrics cron — until those land, allow keeps the foundational
// types compiling clean.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One function's complexity metrics, as produced by a `LanguageBackend`.
///
/// The `function_id` and `file_id` fields are placeholders (`0`) at extraction
/// time; the cron resolves them via `file_symbols` lookup before persisting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionMetrics {
    pub function_id: i64,
    pub file_id: i64,
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub halstead: HalsteadCounts,
    pub npath: NPathValue,
    pub loc: u32,
    pub comment_lines: u32,
    pub panic_paths: u32,
    pub unsafe_blocks: u32,
}

/// Halstead operator/operand counts: distinct (η1/η2) and total (N1/N2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HalsteadCounts {
    /// Distinct operators (η1).
    pub n1: u32,
    /// Distinct operands (η2).
    pub n2: u32,
    /// Total operator occurrences (N1).
    pub big_n1: u32,
    /// Total operand occurrences (N2).
    pub big_n2: u32,
}

impl HalsteadCounts {
    /// Program length N = N1 + N2.
    pub fn length(&self) -> u64 {
        self.big_n1 as u64 + self.big_n2 as u64
    }

    /// Program vocabulary η = η1 + η2.
    pub fn vocabulary(&self) -> u32 {
        self.n1 + self.n2
    }

    /// Volume V = N · log2(η). Zero when vocabulary is zero.
    pub fn volume(&self) -> f64 {
        let eta = self.vocabulary();
        if eta == 0 {
            0.0
        } else {
            self.length() as f64 * (eta as f64).log2()
        }
    }

    /// Difficulty D = (η1 / 2) · (N2 / η2). Zero when either η is zero.
    pub fn difficulty(&self) -> f64 {
        if self.n1 == 0 || self.n2 == 0 {
            0.0
        } else {
            (self.n1 as f64 / 2.0) * (self.big_n2 as f64 / self.n2 as f64)
        }
    }

    /// Effort E = D · V.
    pub fn effort(&self) -> f64 {
        self.difficulty() * self.volume()
    }

    /// Estimated delivered bugs B = V / 3000 (Halstead's empirical constant).
    pub fn bugs(&self) -> f64 {
        self.volume() / 3000.0
    }
}

/// NPath: product of branch factors. Capped at `i64::MAX` with an overflow flag.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NPathValue {
    /// Computed product within u64 range.
    Counted(u64),
    /// Multiplication overflowed during computation; treat as effectively-infinite.
    Overflowed,
}

impl Default for NPathValue {
    fn default() -> Self {
        NPathValue::Counted(1)
    }
}

impl NPathValue {
    /// Map to the BIGINT we persist — overflow becomes i64::MAX so
    /// `ORDER BY npath DESC` still reports pathological functions at the top.
    pub fn as_db_i64(&self) -> i64 {
        match self {
            NPathValue::Counted(n) => (*n).min(i64::MAX as u64) as i64,
            NPathValue::Overflowed => i64::MAX,
        }
    }

    pub fn overflowed(&self) -> bool {
        matches!(self, NPathValue::Overflowed)
    }
}

/// Builder consumed by the language-agnostic scorer in `complexity.rs`.
///
/// Each language backend walks its AST once, populating this struct. The
/// scorer reads it and produces a fully-realized `FunctionMetrics`.
#[derive(Debug, Default)]
pub struct ScoringInput<'a> {
    pub name: &'a str,
    pub start_line: u32,
    pub end_line: u32,
    /// Cyclomatic decision-point count (CC = 1 + decision_points).
    pub decision_points: u32,
    /// Per-site cognitive increments (each site adds 1 + depth or 1).
    pub cognitive_increments: Vec<CognitiveIncrement>,
    /// Operator distinct → count (sums to N1; len()==η1). String keys are static
    /// per-language tokens.
    pub operators: HashMap<&'a str, u32>,
    /// Operand distinct → count (sums to N2; len()==η2). Owned to allow
    /// per-language interning strategies.
    pub operands: HashMap<String, u32>,
    /// NPath branch factors, multiplied together at scoring time.
    pub npath_factors: Vec<u64>,
    /// Logical lines of code in the function body.
    pub source_lines: u32,
    /// Comment lines within the function body's line range.
    pub comment_lines: u32,
    /// Panic-leaf count (`panic!`, `unwrap`, `expect`, `assert!`, etc.).
    pub panic_paths: u32,
    /// Number of `unsafe { ... }` blocks (Rust-specific; 0 elsewhere).
    pub unsafe_blocks: u32,
}

/// One cognitive-complexity increment at a specific nesting depth.
#[derive(Debug, Clone, Copy)]
pub struct CognitiveIncrement {
    /// Nesting depth (0 = top level inside the function body).
    pub depth: u8,
    pub kind: CognitiveKind,
}

/// The kind of cognitive-complexity event. Determines the score contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CognitiveKind {
    /// Break of linear flow: `break`, `continue`, labeled goto, `return` from
    /// deep nesting. Contributes `+1`.
    BreakInFlow,
    /// Nested condition or loop. Contributes `+(1 + depth)` (so depth-1 nest
    /// inside top-level adds 2).
    NestedCondition,
    /// Change of operator kind in a boolean operator chain
    /// (`a && b || c` switches twice). Contributes `+1`.
    LogicalSequence,
    /// Recursion (function calls itself by name). Contributes `+1`.
    Recursion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halstead_volume_is_zero_when_vocabulary_zero() {
        let h = HalsteadCounts::default();
        assert_eq!(h.volume(), 0.0);
        assert_eq!(h.difficulty(), 0.0);
        assert_eq!(h.effort(), 0.0);
        assert_eq!(h.bugs(), 0.0);
    }

    #[test]
    fn halstead_volume_matches_formula() {
        let h = HalsteadCounts {
            n1: 4,
            n2: 8,
            big_n1: 16,
            big_n2: 24,
        };
        // η = 12, N = 40, V = 40 * log2(12) ≈ 143.4083
        let v = h.volume();
        assert!((v - 40.0 * (12.0_f64.log2())).abs() < 1e-9);
    }

    #[test]
    fn halstead_difficulty_handles_zero_eta2() {
        let h = HalsteadCounts {
            n1: 4,
            n2: 0,
            big_n1: 16,
            big_n2: 0,
        };
        assert_eq!(h.difficulty(), 0.0);
        assert_eq!(h.effort(), 0.0);
    }

    #[test]
    fn npath_overflow_persists_as_i64_max() {
        let n = NPathValue::Overflowed;
        assert_eq!(n.as_db_i64(), i64::MAX);
        assert!(n.overflowed());
    }

    #[test]
    fn npath_counted_caps_at_i64_max() {
        let n = NPathValue::Counted(u64::MAX);
        assert_eq!(n.as_db_i64(), i64::MAX);
        assert!(!n.overflowed());
    }

    #[test]
    fn npath_default_is_one() {
        let n = NPathValue::default();
        assert!(matches!(n, NPathValue::Counted(1)));
    }
}
