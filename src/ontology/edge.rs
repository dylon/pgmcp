//! Closed **relation** and **evidence** vocabularies for the ontology layer.
//!
//! - [`OntologyRelation`] — the hierarchy/membership edge types written into the
//!   freeform `memory_relations.relation_type` column (already a documented
//!   passthrough in [`crate::db::ontology::FREEFORM_EDGE_SOURCES`]). Because that
//!   column is intentionally open, these need **no DB CHECK**; the enum validates
//!   tool-supplied edge types and filters hierarchy traversal.
//! - [`EvidenceKind`] — provenance of a piece of evidence backing a concept /
//!   invariant (Code-Digital-Twin `constrained-by` / `justified-by`). A closed
//!   vocabulary → the DB CHECK on `ontology_concept_evidence.evidence_kind` is
//!   built from [`evidence_sql_in_list`] in `crate::db::migrations::v23_ontology`.

// Foundational ontology vocabulary consumed progressively across Phases 1–11.
// See the note in `facet.rs`.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Hierarchy / membership relations among concepts. Stored as
/// `memory_relations.relation_type` values (freeform passthrough), so they ride
/// the existing unified-graph edge matview with zero schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OntologyRelation {
    /// Subsumption: A `is_a` B ⇒ A is a more specific kind of B (transitive).
    IsA,
    /// Mereology: A `part_of` B ⇒ A is a component of B (transitive).
    PartOf,
    /// SKOS-style: A `broader` B ⇒ B is a broader concept than A.
    Broader,
    /// SKOS-style inverse of [`Self::Broader`].
    Narrower,
    /// Instance membership: A `member_of` a `collection`/category concept.
    MemberOf,
}

impl OntologyRelation {
    pub const ALL: &'static [OntologyRelation] = &[
        Self::IsA,
        Self::PartOf,
        Self::Broader,
        Self::Narrower,
        Self::MemberOf,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IsA => "is_a",
            Self::PartOf => "part_of",
            Self::Broader => "broader",
            Self::Narrower => "narrower",
            Self::MemberOf => "member_of",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == s)
    }

    /// The relations whose transitive closure forms the concept hierarchy
    /// (used by ancestor/descendant traversal + the egglog `is_a*` rules).
    /// `narrower`/`member_of` are excluded: `narrower` is the inverse of
    /// `broader` (closure is computed in the `broader` direction), and
    /// `member_of` is instance→category, not subsumption.
    pub fn is_hierarchical(self) -> bool {
        matches!(self, Self::IsA | Self::PartOf | Self::Broader)
    }
}

/// Provenance kind for evidence attached to a concept / invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A git commit message / diff (`commit_id`).
    Commit,
    /// An architecture decision record under `docs/decisions/`.
    Adr,
    /// A CLAUDE.md / AGENTS.md mandate (`mandate_ref`).
    Mandate,
    /// A source-code comment.
    Comment,
    /// Asserted by an agent via a tool (never auto-`canonical`).
    Agent,
    /// A memory observation.
    Observation,
    /// A finding emitted by an analyzer (security/metrics/prediction/concurrency).
    Finding,
    /// A linked v19 data table (tabular non-code data).
    DataTable,
}

impl EvidenceKind {
    pub const ALL: &'static [EvidenceKind] = &[
        Self::Commit,
        Self::Adr,
        Self::Mandate,
        Self::Comment,
        Self::Agent,
        Self::Observation,
        Self::Finding,
        Self::DataTable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Adr => "adr",
            Self::Mandate => "mandate",
            Self::Comment => "comment",
            Self::Agent => "agent",
            Self::Observation => "observation",
            Self::Finding => "finding",
            Self::DataTable => "data_table",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` list for validating ontology relation types (no DB CHECK; used
/// by the tool layer + tests). Built from [`OntologyRelation::ALL`].
pub fn relation_sql_in_list() -> String {
    crate::tracker::kind::join_quoted(OntologyRelation::ALL.iter().map(|r| r.as_str()))
}

/// SQL `IN (...)` list for the `ontology_concept_evidence.evidence_kind` CHECK.
pub fn evidence_sql_in_list() -> String {
    crate::tracker::kind::join_quoted(EvidenceKind::ALL.iter().map(|k| k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn relation_vocabulary_is_pinned() {
        let got: HashSet<&str> = OntologyRelation::ALL.iter().map(|r| r.as_str()).collect();
        let expected: HashSet<&str> = ["is_a", "part_of", "broader", "narrower", "member_of"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "OntologyRelation vocabulary drifted");
        assert_eq!(OntologyRelation::ALL.len(), 5);
    }

    #[test]
    fn evidence_vocabulary_is_pinned() {
        let got: HashSet<&str> = EvidenceKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = [
            "commit",
            "adr",
            "mandate",
            "comment",
            "agent",
            "observation",
            "finding",
            "data_table",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "EvidenceKind vocabulary drifted");
        assert_eq!(EvidenceKind::ALL.len(), 8);
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for r in OntologyRelation::ALL {
            assert_eq!(OntologyRelation::parse(r.as_str()), Some(*r));
        }
        assert_eq!(OntologyRelation::parse("nonsense"), None);
        for k in EvidenceKind::ALL {
            assert_eq!(EvidenceKind::parse(k.as_str()), Some(*k));
        }
    }

    #[test]
    fn hierarchical_relations_are_isa_partof_broader() {
        assert!(OntologyRelation::IsA.is_hierarchical());
        assert!(OntologyRelation::PartOf.is_hierarchical());
        assert!(OntologyRelation::Broader.is_hierarchical());
        assert!(!OntologyRelation::Narrower.is_hierarchical());
        assert!(!OntologyRelation::MemberOf.is_hierarchical());
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let r = relation_sql_in_list();
        assert!(r.starts_with("'is_a'"), "got: {r}");
        assert_eq!(r.matches('\'').count(), OntologyRelation::ALL.len() * 2);
        assert_eq!(r.matches(',').count(), OntologyRelation::ALL.len() - 1);

        let e = evidence_sql_in_list();
        assert!(e.contains("'data_table'") && e.contains("'finding'"));
        assert_eq!(e.matches('\'').count(), EvidenceKind::ALL.len() * 2);
        assert_eq!(e.matches(',').count(), EvidenceKind::ALL.len() - 1);
    }
}
