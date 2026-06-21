//! Closed vocabularies for the scientific-experiment subsystem, following the
//! ADR-003 closed-set idiom: a `TEXT` column + a `CHECK` built from a closed
//! Rust enum's [`sql_in_list`](ExperimentKind::sql_in_list), with a
//! `#[cfg(test)]` golden test pinning each vocabulary — the same idiom as
//! [`crate::tracker::severity`] / [`crate::tracker::kind`] and
//! [`crate::tools_catalog::ToolDomain`].
//!
//! These replace the five native PostgreSQL `ENUM` types
//! (`experiment_kind`, `experiment_status`, `hypothesis_verdict`,
//! `experiment_arm_kind`, `effect_direction`) that the experiment schema used
//! before the `v36_experiment_enum_to_text` migration. Native enums forced a
//! `col = $n::enumtype` cast at every comparison; forgetting it produced the
//! `operator does not exist: experiment_kind = text` class of runtime failure.
//! The closed-set idiom (TEXT + CHECK + this enum) makes that class impossible.
//!
//! Design note: ADR-003's *catalog* path (TEXT[] + a catalog table) is for the
//! unbounded, orthogonal type-tag/effect vocabulary. These five are *closed,
//! single-valued* vocabularies, so they take the closed-set (CHECK-from-enum)
//! sibling — the same path `Severity`/`BugResolution`/`ResolutionKind`/
//! `ToolDomain` take.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// What an experiment is investigating. Stored in `experiments.kind`
/// (default `other`). The label order matches the prior `experiment_kind`
/// `CREATE TYPE` so existing rows round-trip unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentKind {
    Optimization,
    FeatureRefactor,
    FeatureAddition,
    Bugfix,
    Investigation,
    Other,
}

impl ExperimentKind {
    pub const ALL: &'static [ExperimentKind] = &[
        Self::Optimization,
        Self::FeatureRefactor,
        Self::FeatureAddition,
        Self::Bugfix,
        Self::Investigation,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Optimization => "optimization",
            Self::FeatureRefactor => "feature_refactor",
            Self::FeatureAddition => "feature_addition",
            Self::Bugfix => "bugfix",
            Self::Investigation => "investigation",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list — the single source of truth shared with the
    /// `experiments_kind_check` constraint.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Lifecycle state of an experiment. Stored in `experiments.status`
/// (default `open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Open,
    Measuring,
    Decided,
    Abandoned,
    Superseded,
}

impl ExperimentStatus {
    pub const ALL: &'static [ExperimentStatus] = &[
        Self::Open,
        Self::Measuring,
        Self::Decided,
        Self::Abandoned,
        Self::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Measuring => "measuring",
            Self::Decided => "decided",
            Self::Abandoned => "abandoned",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the `experiments_status_check`
    /// constraint.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Lifecycle status of an `experiment_runs` row (closed vocab, ADR-003). A
/// measurement run is `Pending` while open for chunked ingestion, `Complete` once
/// its samples are recorded, `Finalized` when explicitly sealed (the chunked
/// ingestion is committed and conformance-checked), and `Invalid`/`Superseded`
/// when an operator excludes it from decisions — always with a recorded reason,
/// appended immutably to `experiment_run_status_audit`.
///
/// ANTI-TAMPERING: `experiment_decide` consumes ONLY runs where
/// [`usable_in_decision`](Self::usable_in_decision) is true (`complete`/`finalized`)
/// — never `invalid`/`superseded`/`pending` — so a nonconforming or operator-excluded
/// run cannot silently skew a verdict, and a post-decision invalidation is required
/// to re-open the decision rather than quietly dropping unfavorable data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRunStatus {
    Pending,
    Complete,
    Finalized,
    Invalid,
    Superseded,
}

impl ExperimentRunStatus {
    pub const ALL: &'static [ExperimentRunStatus] = &[
        Self::Pending,
        Self::Complete,
        Self::Finalized,
        Self::Invalid,
        Self::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Finalized => "finalized",
            Self::Invalid => "invalid",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// Whether a run with this status may contribute samples to a decision.
    /// `experiment_decide` MUST gate on this (the conformance/anti-tamper rule).
    pub fn usable_in_decision(self) -> bool {
        matches!(self, Self::Complete | Self::Finalized)
    }

    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }

    /// SQL `IN (...)` list of the statuses usable in a decision — derived from
    /// [`usable_in_decision`](Self::usable_in_decision) so the conformance gate
    /// and the vocabulary cannot drift. Used by `load_experiment_samples` and
    /// `experiment_decide` (the anti-tamper rule: only conforming, non-excluded
    /// runs contribute samples to a verdict).
    pub fn usable_in_decision_sql_list() -> String {
        join_quoted(
            Self::ALL
                .iter()
                .copied()
                .filter(|s| s.usable_in_decision())
                .map(|s| s.as_str()),
        )
    }
}

/// Verdict on an experiment hypothesis. Stored in
/// `experiment_hypotheses.verdict` (default `pending`) and
/// `experiment_results.verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisVerdict {
    Pending,
    Accepted,
    Rejected,
    Inconclusive,
}

impl HypothesisVerdict {
    pub const ALL: &'static [HypothesisVerdict] = &[
        Self::Pending,
        Self::Accepted,
        Self::Rejected,
        Self::Inconclusive,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the
    /// `experiment_hypotheses_verdict_check` /
    /// `experiment_results_verdict_check` constraints.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Role of an experiment run within its design. Stored in
/// `experiment_runs.arm_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentArmKind {
    Control,
    Treatment,
    Baseline,
}

impl ExperimentArmKind {
    pub const ALL: &'static [ExperimentArmKind] = &[Self::Control, Self::Treatment, Self::Baseline];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Treatment => "treatment",
            Self::Baseline => "baseline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the `experiment_runs_arm_kind_check`
    /// constraint.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Predicted direction of a hypothesis's effect. Stored in
/// `experiment_hypotheses.predicted_direction` (default `either`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDirection {
    Increase,
    Decrease,
    Either,
    None,
}

impl EffectDirection {
    pub const ALL: &'static [EffectDirection] =
        &[Self::Increase, Self::Decrease, Self::Either, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Increase => "increase",
            Self::Decrease => "decrease",
            Self::Either => "either",
            Self::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the
    /// `experiment_hypotheses_predicted_direction_check` constraint.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Each vocabulary is pinned: the `as_str()` set must exactly match the
    /// labels of the dropped native enum, so existing rows (already TEXT after
    /// the migration) stay valid against the new CHECK.
    #[test]
    fn vocabularies_are_pinned() {
        fn check(got: HashSet<&str>, expected: &[&str]) {
            let exp: HashSet<&str> = expected.iter().copied().collect();
            assert_eq!(got, exp, "experiment vocabulary drifted from pinned set");
            assert_eq!(got.len(), expected.len(), "duplicate as_str() value");
        }
        check(
            ExperimentKind::ALL.iter().map(|x| x.as_str()).collect(),
            &[
                "optimization",
                "feature_refactor",
                "feature_addition",
                "bugfix",
                "investigation",
                "other",
            ],
        );
        check(
            ExperimentStatus::ALL.iter().map(|x| x.as_str()).collect(),
            &["open", "measuring", "decided", "abandoned", "superseded"],
        );
        check(
            HypothesisVerdict::ALL.iter().map(|x| x.as_str()).collect(),
            &["pending", "accepted", "rejected", "inconclusive"],
        );
        check(
            ExperimentArmKind::ALL.iter().map(|x| x.as_str()).collect(),
            &["control", "treatment", "baseline"],
        );
        check(
            EffectDirection::ALL.iter().map(|x| x.as_str()).collect(),
            &["increase", "decrease", "either", "none"],
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for x in ExperimentKind::ALL {
            assert_eq!(ExperimentKind::parse(x.as_str()), Some(*x));
        }
        for x in ExperimentStatus::ALL {
            assert_eq!(ExperimentStatus::parse(x.as_str()), Some(*x));
        }
        for x in HypothesisVerdict::ALL {
            assert_eq!(HypothesisVerdict::parse(x.as_str()), Some(*x));
        }
        for x in ExperimentArmKind::ALL {
            assert_eq!(ExperimentArmKind::parse(x.as_str()), Some(*x));
        }
        for x in EffectDirection::ALL {
            assert_eq!(EffectDirection::parse(x.as_str()), Some(*x));
        }
        assert_eq!(ExperimentKind::parse("nonsense"), None);
        assert_eq!(ExperimentStatus::parse("nonsense"), None);
        assert_eq!(HypothesisVerdict::parse("nonsense"), None);
        assert_eq!(ExperimentArmKind::parse("nonsense"), None);
        assert_eq!(EffectDirection::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        for (list, n, first) in [
            (
                ExperimentKind::sql_in_list(),
                ExperimentKind::ALL.len(),
                "'optimization'",
            ),
            (
                ExperimentStatus::sql_in_list(),
                ExperimentStatus::ALL.len(),
                "'open'",
            ),
            (
                HypothesisVerdict::sql_in_list(),
                HypothesisVerdict::ALL.len(),
                "'pending'",
            ),
            (
                ExperimentArmKind::sql_in_list(),
                ExperimentArmKind::ALL.len(),
                "'control'",
            ),
            (
                EffectDirection::sql_in_list(),
                EffectDirection::ALL.len(),
                "'increase'",
            ),
        ] {
            assert!(list.starts_with(first), "got: {list}");
            assert_eq!(list.matches('\'').count(), n * 2, "quote count: {list}");
            assert_eq!(list.matches(',').count(), n - 1, "comma count: {list}");
        }
    }
}
