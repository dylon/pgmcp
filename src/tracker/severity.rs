//! Closed bug-tracking vocabularies: [`Severity`] (the *impact* axis, orthogonal
//! to `work_items.priority` which is *urgency*) and [`BugResolution`] (the
//! categorized terminal outcome of a bug that is closed without the evidence-
//! backed verify path). Per ADR-003 each is a `TEXT` column + a `CHECK` built
//! from a closed Rust enum via [`sql_in_list`], with a `#[cfg(test)]` golden
//! test pinning the vocabulary — the same idiom as [`crate::tracker::kind`] and
//! [`crate::tracker::status`].

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// How bad a defect is when it manifests — the *impact* axis. Stored nullable in
/// `work_items.severity` (only `kind = 'bug'` items carry it). Distinct from
/// `priority` (0–100), which ranks *urgency*: a low-severity bug can be high
/// priority and vice-versa. When a severity is set and no explicit priority was
/// given, [`Severity::default_priority`] seeds a sensible urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Data loss, security breach, or total outage — drop everything.
    Critical,
    /// Major functionality broken with no workaround.
    High,
    /// Functionality impaired but with a workaround.
    Medium,
    /// Minor / cosmetic; little user impact.
    Low,
}

impl Severity {
    /// Canonical ordering (highest impact first); also the source of the DB
    /// CHECK vocabulary.
    pub const ALL: &'static [Severity] = &[Self::Critical, Self::High, Self::Medium, Self::Low];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|sev| sev.as_str() == s)
    }

    /// Ordinal impact rank (Critical = 4 … Low = 1) for sorting / comparison.
    /// Part of the closed `Severity` surface (paired with [`Severity::default_priority`]);
    /// `#[allow(dead_code)]` documents that it is a deliberate API member with no
    /// internal caller yet — the same idiom as `Actor::parse` in
    /// [`crate::tracker::transition`] — and it is exercised by the unit tests.
    #[allow(dead_code)]
    pub fn rank(self) -> u8 {
        match self {
            Self::Critical => 4,
            Self::High => 3,
            Self::Medium => 2,
            Self::Low => 1,
        }
    }

    /// Urgency seed applied to `work_items.priority` (0–100) when a severity is
    /// set but no explicit priority was supplied. Never clobbers an explicit
    /// priority — the caller only applies this when `priority` was omitted.
    pub fn default_priority(self) -> i32 {
        match self {
            Self::Critical => 90,
            Self::High => 70,
            Self::Medium => 40,
            Self::Low => 20,
        }
    }
}

/// SQL `IN (...)` value list built from [`Severity::ALL`] — the single source of
/// truth shared with the `work_items_severity_check` constraint.
pub fn sql_in_list() -> String {
    join_quoted(Severity::ALL.iter().map(|s| s.as_str()))
}

/// The categorized terminal outcome of a bug. The four non-[`Fixed`] values are
/// recorded by `work_item_resolve` when a bug is closed (→ `cancelled`) without
/// the evidence-backed verify path; [`Fixed`] is reserved for reporting and is
/// derived from `status = 'verified' ∧ kind = 'bug'` (the verify path is left
/// untouched). Stored in `work_item_bug_details.resolution`.
///
/// [`Fixed`]: BugResolution::Fixed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BugResolution {
    /// The defect was fixed and the fix verified (derived from `verified`
    /// status; never written by `work_item_resolve`).
    Fixed,
    /// A real defect we have decided not to fix.
    WontFix,
    /// Already tracked by another work item (paired with a `duplicates`
    /// relation).
    Duplicate,
    /// Could not be reproduced from the report.
    CannotReproduce,
    /// The observed behavior is intentional / by design.
    ByDesign,
}

impl BugResolution {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [BugResolution] = &[
        Self::Fixed,
        Self::WontFix,
        Self::Duplicate,
        Self::CannotReproduce,
        Self::ByDesign,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::WontFix => "wont_fix",
            Self::Duplicate => "duplicate",
            Self::CannotReproduce => "cannot_reproduce",
            Self::ByDesign => "by_design",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == s)
    }

    /// Whether this resolution may be set by `work_item_resolve` (everything
    /// except [`BugResolution::Fixed`], which is derived from the verify path).
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Fixed)
    }
}

/// SQL `IN (...)` value list built from [`BugResolution::ALL`] — the single
/// source of truth shared with the `work_item_bug_details_resolution_check`
/// constraint.
pub fn resolution_sql_in_list() -> String {
    join_quoted(BugResolution::ALL.iter().map(|r| r.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn severity_vocabulary_is_pinned() {
        let got: HashSet<&str> = Severity::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = ["critical", "high", "medium", "low"].into_iter().collect();
        assert_eq!(got, expected, "Severity vocabulary drifted from pinned set");
        assert_eq!(Severity::ALL.len(), 4);
        assert_eq!(got.len(), 4, "duplicate as_str() value in Severity");
    }

    #[test]
    fn bug_resolution_vocabulary_is_pinned() {
        let got: HashSet<&str> = BugResolution::ALL.iter().map(|r| r.as_str()).collect();
        let expected: HashSet<&str> = [
            "fixed",
            "wont_fix",
            "duplicate",
            "cannot_reproduce",
            "by_design",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "BugResolution vocabulary drifted from pinned set"
        );
        assert_eq!(BugResolution::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() value in BugResolution");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for s in Severity::ALL {
            assert_eq!(Severity::parse(s.as_str()), Some(*s));
        }
        assert_eq!(Severity::parse("nonsense"), None);
        for r in BugResolution::ALL {
            assert_eq!(BugResolution::parse(r.as_str()), Some(*r));
        }
        assert_eq!(BugResolution::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.starts_with("'critical'"), "got: {s}");
        assert!(s.contains("'low'"));
        assert_eq!(s.matches('\'').count(), Severity::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), Severity::ALL.len() - 1);

        let r = resolution_sql_in_list();
        assert!(r.contains("'wont_fix'"));
        assert_eq!(r.matches('\'').count(), BugResolution::ALL.len() * 2);
        assert_eq!(r.matches(',').count(), BugResolution::ALL.len() - 1);
    }

    #[test]
    fn default_priority_within_bounds() {
        for s in Severity::ALL {
            let p = s.default_priority();
            assert!(
                (0..=100).contains(&p),
                "{s:?} default_priority {p} out of 0..=100"
            );
        }
        // Higher impact ⇒ higher (or equal) default urgency.
        assert!(Severity::Critical.default_priority() > Severity::Low.default_priority());
    }

    #[test]
    fn rank_is_strictly_ordered() {
        assert!(Severity::Critical.rank() > Severity::High.rank());
        assert!(Severity::High.rank() > Severity::Medium.rank());
        assert!(Severity::Medium.rank() > Severity::Low.rank());
    }

    #[test]
    fn only_fixed_is_not_user_settable() {
        for r in BugResolution::ALL {
            assert_eq!(r.is_user_settable(), *r != BugResolution::Fixed);
        }
    }
}
