//! Closed inter-experiment relation vocabulary — the structural twin of
//! `crate::tracker` item-relation types, for the `experiment_relations` DAG
//! (experiment B replicates / refutes / extends … experiment A).
//!
//! Per ADR-003, a closed/evolvable-but-known vocabulary is modeled as `TEXT` +
//! `CHECK` + a closed Rust enum that is the single source of truth. The DB
//! CHECK on `experiment_relations.relation_type` is built from [`sql_in_list`]
//! in `crate::db::migrations::v6_unified_graph`, and a `#[cfg(test)]` golden
//! test pins the vocabulary. (`experiments.superseded_by` is only a bitemporal
//! version chain — it cannot express cross-experiment scientific relations, so
//! this table/vocabulary exists alongside it.)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRelation {
    /// This experiment reproduces an earlier one's protocol/result.
    Replicates,
    /// This experiment contradicts an earlier one's conclusion.
    Refutes,
    /// This experiment builds on / generalizes an earlier one.
    Extends,
    /// This experiment obsoletes an earlier one (distinct from the bitemporal
    /// `superseded_by` self-version chain: that is the *same* experiment re-run;
    /// this is a *different* experiment that supersedes another).
    Supersedes,
    /// Loosely related (catch-all).
    RelatesTo,
    /// This experiment was derived from an earlier one.
    DerivedFrom,
}

impl ExperimentRelation {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [ExperimentRelation] = &[
        Self::Replicates,
        Self::Refutes,
        Self::Extends,
        Self::Supersedes,
        Self::RelatesTo,
        Self::DerivedFrom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replicates => "replicates",
            Self::Refutes => "refutes",
            Self::Extends => "extends",
            Self::Supersedes => "supersedes",
            Self::RelatesTo => "relates_to",
            Self::DerivedFrom => "derived_from",
        }
    }

    // Input parser, symmetric with `WorkItemKind::parse`. Used to validate a
    // caller-supplied `relation_type` in the Stage-2 experiment-relation linking
    // tool; the migration vocabulary itself flows through `sql_in_list`.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` value list (e.g. `'replicates','refutes',...`) built from
/// [`ExperimentRelation::ALL`] — the single source of truth shared with the
/// `experiment_relations_type_check` migration constraint. Reuses the shared
/// `join_quoted` helper from `crate::tracker::kind`.
pub fn sql_in_list() -> String {
    crate::tracker::kind::join_quoted(ExperimentRelation::ALL.iter().map(|k| k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn relation_vocabulary_is_pinned() {
        let got: HashSet<&str> = ExperimentRelation::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = [
            "replicates",
            "refutes",
            "extends",
            "supersedes",
            "relates_to",
            "derived_from",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "ExperimentRelation vocabulary drifted from pinned set"
        );
        assert_eq!(ExperimentRelation::ALL.len(), 6);
        assert_eq!(
            got.len(),
            6,
            "duplicate as_str() value in ExperimentRelation"
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for r in ExperimentRelation::ALL {
            assert_eq!(ExperimentRelation::parse(r.as_str()), Some(*r));
        }
        assert_eq!(ExperimentRelation::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.starts_with("'replicates'"), "got: {s}");
        assert!(s.contains("'derived_from'"));
        assert_eq!(s.matches('\'').count(), ExperimentRelation::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), ExperimentRelation::ALL.len() - 1);
    }
}
