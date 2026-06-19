//! Category-theoretic layer over the workspace graph (ADR-028, item 4).
//!
//! The workspace is modeled as a small set of categories grounded in real tables
//! (objects / morphisms):
//!
//! | Category        | Objects                | Morphisms                          |
//! |-----------------|------------------------|------------------------------------|
//! | **Call**        | functions (`file_symbols`) | `symbol_references` call edges  |
//! | **FileDep**     | files (`indexed_files`)    | `import` edges                  |
//! | **ProjectDep**  | projects               | `project_dependencies`             |
//! | **Containment** | the `HierLevel` chain  | the rollup functor `symbol→…→workspace` |
//!
//! The **Containment functor** carries metrics up the chain (`hierarchy::rollup`).
//! A functor must preserve composition; whether a given metric *does* depends on
//! whether it is **extensive** (a sum — preserved) or **intensive** (a mean —
//! only approximately preserved). That is the `RollupLaw` distinction, and
//! `categorical_lint` checks the strict (extensive) laws as data-integrity
//! invariants: a violation is a real bug, not a modeling choice.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Whether a rolled-up metric is composition-preserving under the Containment
/// functor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupLaw {
    /// Extensive: the parent equals the SUM of children (e.g. file counts).
    /// `Σ_workspace == Σ_projects(Σ_modules(...))` must hold exactly — a mismatch
    /// is a data-integrity bug.
    Strict,
    /// Intensive: the parent is a (weighted) MEAN of children (e.g. instability).
    /// Not composition-preserving in general; reported honestly, never asserted.
    Lax,
}

impl RollupLaw {
    pub const ALL: &'static [RollupLaw] = &[Self::Strict, Self::Lax];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Lax => "lax",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
}

/// A strict extensive-sum invariant to check: the workspace total of `column`
/// must equal the sum over `project_metrics`. Each is a composition law of the
/// Containment functor; a mismatch is a real bug surfaced by `categorical_lint`.
pub struct StrictLaw {
    pub name: &'static str,
    /// Column on both `project_metrics` and `hier_group_metrics`.
    pub column: &'static str,
}

/// The strict laws checked by `categorical_lint`. Extensive sums only — these are
/// the columns that roll up by addition (file counts) and so must be preserved
/// exactly by the functor.
pub const STRICT_LAWS: &[StrictLaw] = &[StrictLaw {
    name: "file_count_extensive",
    column: "file_count",
}];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_law_roundtrips() {
        for l in RollupLaw::ALL {
            assert_eq!(RollupLaw::parse(l.as_str()), Some(*l));
        }
        assert_eq!(RollupLaw::parse("nope"), None);
    }

    #[test]
    fn strict_laws_are_nonempty_and_named() {
        assert!(!STRICT_LAWS.is_empty());
        assert!(
            STRICT_LAWS
                .iter()
                .all(|l| !l.name.is_empty() && !l.column.is_empty())
        );
    }
}
