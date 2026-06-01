//! Closed vocabularies for the hierarchical ontology layer — the **facet** a
//! concept is classified along, and its curation **status**.
//!
//! Per ADR-003, a closed/evolvable-but-known vocabulary is modeled as `TEXT` +
//! `CHECK` + a closed Rust enum that is the single source of truth. The DB
//! `CHECK`s on `ontology_concept_meta.facet` / `.status` are built from
//! [`facet_sql_in_list`] / [`status_sql_in_list`] in
//! `crate::db::migrations::v23_ontology`, and the `#[cfg(test)]` golden tests
//! below pin the vocabularies. New variants are *additive*: append the variant,
//! its `as_str`, and bump the golden count.

// Foundational ontology vocabulary consumed progressively across Phases 1–11
// (queries / tools / crons). Allowed module-wide until fully wired; tightened to
// item-level once every variant + helper has a non-test caller.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// The dimension a concept is classified along. `domain_concept` is the
/// catch-all; the final four (`tool`/`system`/`resource`/`collection`) serve
/// hand-curated, non-code categories (a `collection` "Formal Verification
/// Systems" whose members are `tool`/`system` concepts; a `resource` "Hardware"
/// carrying a data table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facet {
    Architecture,
    Component,
    Algorithm,
    DataStructure,
    Paradigm,
    DesignPattern,
    EngineeringPractice,
    Strategy,
    Security,
    Concurrency,
    Protocol,
    DomainConcept,
    Invariant,
    Tool,
    System,
    Resource,
    Collection,
}

impl Facet {
    /// Canonical ordering; also the source of the DB `CHECK` vocabulary.
    pub const ALL: &'static [Facet] = &[
        Self::Architecture,
        Self::Component,
        Self::Algorithm,
        Self::DataStructure,
        Self::Paradigm,
        Self::DesignPattern,
        Self::EngineeringPractice,
        Self::Strategy,
        Self::Security,
        Self::Concurrency,
        Self::Protocol,
        Self::DomainConcept,
        Self::Invariant,
        Self::Tool,
        Self::System,
        Self::Resource,
        Self::Collection,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Architecture => "architecture",
            Self::Component => "component",
            Self::Algorithm => "algorithm",
            Self::DataStructure => "data_structure",
            Self::Paradigm => "paradigm",
            Self::DesignPattern => "design_pattern",
            Self::EngineeringPractice => "engineering_practice",
            Self::Strategy => "strategy",
            Self::Security => "security",
            Self::Concurrency => "concurrency",
            Self::Protocol => "protocol",
            Self::DomainConcept => "domain_concept",
            Self::Invariant => "invariant",
            Self::Tool => "tool",
            Self::System => "system",
            Self::Resource => "resource",
            Self::Collection => "collection",
        }
    }

    /// Input parser, symmetric with the other ADR-003 enums.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == s)
    }
}

/// Curation lifecycle of a concept. `accepted` / `canonical` are **curator-only**
/// (a non-`agent` actor): an agent may author `candidate`s and propose, but the
/// `set_concept_status` chokepoint (`crate::db::queries::ontology`) refuses an
/// agent→`accepted`/`canonical` transition — the structural trust boundary that
/// mirrors the work-item tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConceptStatus {
    Candidate,
    Accepted,
    Canonical,
    Deprecated,
}

impl ConceptStatus {
    pub const ALL: &'static [ConceptStatus] = &[
        Self::Candidate,
        Self::Accepted,
        Self::Canonical,
        Self::Deprecated,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Accepted => "accepted",
            Self::Canonical => "canonical",
            Self::Deprecated => "deprecated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s2| s2.as_str() == s)
    }

    /// `true` for statuses an agent must NOT self-assign — enforced at the tool /
    /// query chokepoint, not the DB (the DB `CHECK` only pins the vocabulary).
    pub fn is_curator_only(self) -> bool {
        matches!(self, Self::Accepted | Self::Canonical)
    }
}

/// SQL `IN (...)` value list for the `ontology_concept_meta.facet` CHECK,
/// built from [`Facet::ALL`] (the single source of truth).
pub fn facet_sql_in_list() -> String {
    crate::tracker::kind::join_quoted(Facet::ALL.iter().map(|f| f.as_str()))
}

/// SQL `IN (...)` value list for the `ontology_concept_meta.status` CHECK.
pub fn status_sql_in_list() -> String {
    crate::tracker::kind::join_quoted(ConceptStatus::ALL.iter().map(|s| s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn facet_vocabulary_is_pinned() {
        let got: HashSet<&str> = Facet::ALL.iter().map(|f| f.as_str()).collect();
        let expected: HashSet<&str> = [
            "architecture",
            "component",
            "algorithm",
            "data_structure",
            "paradigm",
            "design_pattern",
            "engineering_practice",
            "strategy",
            "security",
            "concurrency",
            "protocol",
            "domain_concept",
            "invariant",
            "tool",
            "system",
            "resource",
            "collection",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "Facet vocabulary drifted from pinned set");
        assert_eq!(Facet::ALL.len(), 17);
        assert_eq!(got.len(), 17, "duplicate as_str() value in Facet");
    }

    #[test]
    fn status_vocabulary_is_pinned() {
        let got: HashSet<&str> = ConceptStatus::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = ["candidate", "accepted", "canonical", "deprecated"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "ConceptStatus vocabulary drifted");
        assert_eq!(ConceptStatus::ALL.len(), 4);
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for f in Facet::ALL {
            assert_eq!(Facet::parse(f.as_str()), Some(*f));
        }
        assert_eq!(Facet::parse("nonsense"), None);
        for s in ConceptStatus::ALL {
            assert_eq!(ConceptStatus::parse(s.as_str()), Some(*s));
        }
        assert_eq!(ConceptStatus::parse("nonsense"), None);
    }

    #[test]
    fn curator_only_is_exactly_accepted_and_canonical() {
        assert!(ConceptStatus::Accepted.is_curator_only());
        assert!(ConceptStatus::Canonical.is_curator_only());
        assert!(!ConceptStatus::Candidate.is_curator_only());
        assert!(!ConceptStatus::Deprecated.is_curator_only());
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let f = facet_sql_in_list();
        assert!(f.starts_with("'architecture'"), "got: {f}");
        assert!(f.contains("'collection'"));
        assert_eq!(f.matches('\'').count(), Facet::ALL.len() * 2);
        assert_eq!(f.matches(',').count(), Facet::ALL.len() - 1);

        let s = status_sql_in_list();
        assert!(s.contains("'candidate'") && s.contains("'canonical'"));
        assert_eq!(s.matches('\'').count(), ConceptStatus::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), ConceptStatus::ALL.len() - 1);
    }
}
