//! Closed vocabulary for symbol-reference resolution tiers (shadow-ASR).
//!
//! `resolve_symbol_reference_targets` (`src/db/queries/symbols.rs`) classifies
//! every `symbol_references` row into one of these tiers and writes the value
//! into `symbol_references.resolution_kind`, paired with the matching
//! [`ResolutionKind::confidence`] in `resolution_confidence`.
//!
//! Per ADR-003 the column is `TEXT` + a `CHECK` built from
//! [`sql_in_list`], with a `#[cfg(test)]` golden test pinning the vocabulary —
//! the same idiom as [`crate::tracker::severity::Severity`]. The constraint is
//! installed by the `v14_resolution_kind_vocab` migration; emitting the enum's
//! `as_db_str()` from the resolver (rather than string literals) makes a typo a
//! compile-time error and keeps writer ⇄ CHECK in lockstep.
//!
//! Kept self-contained (no `crate::tracker` dependency) so the parsing layer
//! does not depend on the work-item tracker.

use serde::{Deserialize, Serialize};

/// How a `symbol_references` row's `target_raw` was resolved to a project
/// symbol. Ordered most-precise-first; [`ResolutionKind::ALL`] is the source of
/// the DB CHECK vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionKind {
    /// `target_raw` matches a symbol defined in the *same* file (confidence 1.0).
    ExactInFile,
    /// `target_raw` resolves through an `import` edge to a symbol in another
    /// file within the project (confidence 0.95).
    ExactViaImport,
    /// Bare-name match with exactly one project-wide candidate — almost
    /// certainly correct (confidence 0.7).
    BareNameUnique,
    /// Bare-name match with multiple candidates; the DB picks one but it is an
    /// unreliable guess, so downstream weighting can discount it (confidence 0.3).
    BareNameAmbiguous,
    /// Target is outside the project (third-party / stdlib). Reserved for a
    /// future external-symbol pass; not emitted by the current resolver
    /// (confidence 0.0).
    External,
    /// No project symbol matched `target_raw` (confidence 0.0,
    /// `target_symbol_id` NULL).
    Unresolved,
}

impl ResolutionKind {
    /// Canonical ordering (most precise first); also the source of the DB CHECK
    /// vocabulary and the `resolution_confidence` ladder.
    pub const ALL: &'static [ResolutionKind] = &[
        Self::ExactInFile,
        Self::ExactViaImport,
        Self::BareNameUnique,
        Self::BareNameAmbiguous,
        Self::External,
        Self::Unresolved,
    ];

    /// Stable string stored in `symbol_references.resolution_kind`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::ExactInFile => "exact_in_file",
            Self::ExactViaImport => "exact_via_import",
            Self::BareNameUnique => "bare_name_unique",
            Self::BareNameAmbiguous => "bare_name_ambiguous",
            Self::External => "external",
            Self::Unresolved => "unresolved",
        }
    }

    /// Parse a DB string back into the enum. Part of the closed surface; used by
    /// traversal/query layers that read `resolution_kind` back (Phase 1+), so
    /// `#[allow(dead_code)]` documents it as a deliberate API member — the same
    /// idiom as `Severity::rank`.
    #[allow(dead_code)]
    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_db_str() == s)
    }

    /// The `resolution_confidence` paired with this tier, in `[0.0, 1.0]`. The
    /// single source of truth for the confidence the resolver writes alongside
    /// `resolution_kind` — keeps the CASE arms in
    /// `resolve_symbol_reference_targets` from drifting from the tier meaning.
    pub fn confidence(self) -> f32 {
        match self {
            Self::ExactInFile => 1.0,
            Self::ExactViaImport => 0.95,
            Self::BareNameUnique => 0.7,
            Self::BareNameAmbiguous => 0.3,
            Self::External | Self::Unresolved => 0.0,
        }
    }
}

/// SQL `IN (...)` value list built from [`ResolutionKind::ALL`] — the single
/// source of truth shared with the `chk_symbol_refs_resolution_kind` constraint
/// (migration `v14_resolution_kind_vocab`).
pub fn sql_in_list() -> String {
    ResolutionKind::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn resolution_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = ResolutionKind::ALL.iter().map(|k| k.as_db_str()).collect();
        let expected: HashSet<&str> = [
            "exact_in_file",
            "exact_via_import",
            "bare_name_unique",
            "bare_name_ambiguous",
            "external",
            "unresolved",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "ResolutionKind vocabulary drifted from pinned set — update the \
             v14_resolution_kind_vocab CHECK and resolve_symbol_reference_targets together"
        );
        assert_eq!(ResolutionKind::ALL.len(), 6);
        assert_eq!(
            got.len(),
            6,
            "duplicate as_db_str() value in ResolutionKind"
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in ResolutionKind::ALL {
            assert_eq!(ResolutionKind::from_db_str(k.as_db_str()), Some(*k));
        }
        assert_eq!(ResolutionKind::from_db_str("bare_name_in_project"), None);
        assert_eq!(ResolutionKind::from_db_str("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.starts_with("'exact_in_file'"), "got: {s}");
        assert!(s.contains("'bare_name_unique'"));
        assert!(s.contains("'bare_name_ambiguous'"));
        assert!(
            !s.contains("bare_name_in_project"),
            "legacy value must be gone"
        );
        assert_eq!(s.matches('\'').count(), ResolutionKind::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), ResolutionKind::ALL.len() - 1);
    }

    #[test]
    fn confidence_within_bounds_and_ordered() {
        for k in ResolutionKind::ALL {
            let c = k.confidence();
            assert!(
                (0.0..=1.0).contains(&c),
                "{k:?} confidence {c} out of [0,1]"
            );
        }
        // More-precise tiers carry strictly higher confidence.
        assert!(
            ResolutionKind::ExactInFile.confidence() > ResolutionKind::ExactViaImport.confidence()
        );
        assert!(
            ResolutionKind::ExactViaImport.confidence()
                > ResolutionKind::BareNameUnique.confidence()
        );
        assert!(
            ResolutionKind::BareNameUnique.confidence()
                > ResolutionKind::BareNameAmbiguous.confidence()
        );
        assert!(
            ResolutionKind::BareNameAmbiguous.confidence()
                > ResolutionKind::Unresolved.confidence()
        );
    }
}
