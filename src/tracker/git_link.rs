//! Closed vocabularies for the Phase-3 git/PR close-the-loop layer:
//! [`GitLinkType`] (the kind of repo artifact a work item is linked to —
//! commit / PR / branch) and [`FindingSource`] (the analytic that auto-promoted
//! a finding into a `pending` work item, the idempotency lineage stored in
//! `work_item_finding_provenance.finding_source`).
//!
//! Per ADR-003 each is a `TEXT` column + a `CHECK` built from a closed Rust enum
//! via [`sql_in_list`], with a `#[cfg(test)]` golden test pinning the
//! vocabulary — the same idiom as [`crate::tracker::kind`],
//! [`crate::tracker::status`], and [`crate::tracker::severity`].

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// The kind of repo artifact a `work_item_git_links` row points at. The
/// `ref_value` column holds the artifact's identifier (a commit SHA, a PR
/// number, or a branch name); `link_type` disambiguates how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitLinkType {
    /// A git commit. `ref_value` is the commit SHA (full or a unique prefix);
    /// the optional `commit_id` FK resolves to a `git_commits` row when the
    /// commit has been indexed for this project.
    Commit,
    /// A pull / merge request. `ref_value` is the PR number (as text) or its
    /// URL slug.
    Pr,
    /// A branch. `ref_value` is the branch name (e.g. `feat/work-item-tracker`).
    Branch,
}

impl GitLinkType {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [GitLinkType] = &[Self::Commit, Self::Pr, Self::Branch];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Pr => "pr",
            Self::Branch => "branch",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }

    /// Infer a link type from the *shape* of a `ref_value` when the caller
    /// omits an explicit `link_type` (the `work_item_link_commit` ergonomic):
    ///
    /// - all-hex and length ≥ 7 ⇒ [`Commit`] (a SHA or unique prefix);
    /// - all-digits (optionally a leading `#`) ⇒ [`Pr`] (a PR number);
    /// - everything else ⇒ [`Branch`] (a branch name).
    ///
    /// [`Commit`]: GitLinkType::Commit
    /// [`Pr`]: GitLinkType::Pr
    /// [`Branch`]: GitLinkType::Branch
    pub fn infer_from_ref(ref_value: &str) -> Self {
        let v = ref_value.trim();
        let digits = v.strip_prefix('#').unwrap_or(v);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return Self::Pr;
        }
        if v.len() >= 7 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Self::Commit;
        }
        Self::Branch
    }
}

/// SQL `IN (...)` value list built from [`GitLinkType::ALL`] — the single source
/// of truth shared with the `work_item_git_links_link_type_check` constraint.
pub fn sql_in_list() -> String {
    join_quoted(GitLinkType::ALL.iter().map(|t| t.as_str()))
}

/// Which analytic produced a finding that was auto-promoted into a `pending`
/// work item. Stored in `work_item_finding_provenance.finding_source`; pairs
/// with the `provenance_key` UNIQUE column to make re-promotion idempotent (the
/// same finding never spawns a second item on a later cron run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSource {
    /// A high-confidence file from the `bug_prediction` model (score ≥ the
    /// configured threshold) → a `pending` `bug` item.
    BugPrediction,
    /// A high-severity developer-authored debt marker (FIXME / BUG / HACK)
    /// surfaced by `documented_tech_debt` → a `pending` `fixme` item.
    DocumentedTechDebt,
    /// A lock-order deadlock cycle (`deadlock_cycles`, ADR-011) → a `pending`
    /// `bug` item.
    DeadlockCycle,
    /// A channel deadlock — `blocked_recv` / `channel_cycle` (ADR-011) → a
    /// `pending` `bug` item.
    ChannelDeadlock,
    /// A high-severity finding from an external security-scanner run over a
    /// project (the `security_scan` cron / tool — gitleaks, semgrep, trivy, …)
    /// → a `pending` `bug` item. The specific scanner is carried in the
    /// `provenance_key` and the `external_scanner_findings.scanner` column, so
    /// this one source value preserves full per-scanner provenance.
    SecurityScan,
}

impl FindingSource {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [FindingSource] = &[
        Self::BugPrediction,
        Self::DocumentedTechDebt,
        Self::DeadlockCycle,
        Self::ChannelDeadlock,
        Self::SecurityScan,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BugPrediction => "bug_prediction",
            Self::DocumentedTechDebt => "documented_tech_debt",
            Self::DeadlockCycle => "deadlock_cycle",
            Self::ChannelDeadlock => "channel_deadlock",
            Self::SecurityScan => "security_scan",
        }
    }

    /// Parse a finding source from its `as_str` form — the symmetric inverse of
    /// [`FindingSource::as_str`], part of the closed-enum surface. The cron
    /// writes `finding_source` via `as_str` and never reads it back in
    /// non-test code yet, so `#[allow(dead_code)]` documents that this is a
    /// deliberate API member (the same idiom as `Actor::parse` in
    /// [`crate::tracker::transition`]); the golden tests exercise it.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == s)
    }

    /// The `work_items.kind` an item promoted from this source is created with.
    /// `bug_prediction` (a defect-prone file) → `bug`; `documented_tech_debt`
    /// (a FIXME/BUG/HACK marker) → `fixme`, the lightweight code-scan marker
    /// kind. Both land in `pending`, never pre-`confirmed` (confirmation is
    /// user-only).
    pub fn item_kind(self) -> &'static str {
        match self {
            Self::BugPrediction => "bug",
            Self::DocumentedTechDebt => "fixme",
            // Deadlock cycles are correctness defects → first-class `bug`.
            Self::DeadlockCycle | Self::ChannelDeadlock => "bug",
            // External security-scanner findings are first-class `bug`s.
            Self::SecurityScan => "bug",
        }
    }
}

/// SQL `IN (...)` value list built from [`FindingSource::ALL`] — the single
/// source of truth shared with the `work_item_finding_provenance_source_check`
/// constraint.
pub fn finding_source_sql_in_list() -> String {
    join_quoted(FindingSource::ALL.iter().map(|f| f.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn git_link_type_vocabulary_is_pinned() {
        let got: HashSet<&str> = GitLinkType::ALL.iter().map(|t| t.as_str()).collect();
        let expected: HashSet<&str> = ["commit", "pr", "branch"].into_iter().collect();
        assert_eq!(
            got, expected,
            "GitLinkType vocabulary drifted from pinned set"
        );
        assert_eq!(GitLinkType::ALL.len(), 3);
        assert_eq!(got.len(), 3, "duplicate as_str() value in GitLinkType");
    }

    #[test]
    fn finding_source_vocabulary_is_pinned() {
        let got: HashSet<&str> = FindingSource::ALL.iter().map(|f| f.as_str()).collect();
        let expected: HashSet<&str> = [
            "bug_prediction",
            "documented_tech_debt",
            "deadlock_cycle",
            "channel_deadlock",
            "security_scan",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "FindingSource vocabulary drifted from pinned set"
        );
        assert_eq!(FindingSource::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() value in FindingSource");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for t in GitLinkType::ALL {
            assert_eq!(GitLinkType::parse(t.as_str()), Some(*t));
        }
        assert_eq!(GitLinkType::parse("nonsense"), None);
        for f in FindingSource::ALL {
            assert_eq!(FindingSource::parse(f.as_str()), Some(*f));
        }
        assert_eq!(FindingSource::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.contains("'commit'"), "got: {s}");
        assert!(s.contains("'pr'"));
        assert!(s.contains("'branch'"));
        assert_eq!(s.matches('\'').count(), GitLinkType::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), GitLinkType::ALL.len() - 1);

        let f = finding_source_sql_in_list();
        assert!(f.contains("'bug_prediction'"));
        assert!(f.contains("'documented_tech_debt'"));
        assert_eq!(f.matches('\'').count(), FindingSource::ALL.len() * 2);
        assert_eq!(f.matches(',').count(), FindingSource::ALL.len() - 1);
    }

    #[test]
    fn infer_from_ref_classifies_shapes() {
        // SHA / hex prefix ≥ 7 ⇒ commit
        assert_eq!(
            GitLinkType::infer_from_ref("0f3e647a1b2c3d4e5f"),
            GitLinkType::Commit
        );
        assert_eq!(GitLinkType::infer_from_ref("0f3e647"), GitLinkType::Commit);
        // digits (optionally #) ⇒ pr
        assert_eq!(GitLinkType::infer_from_ref("123"), GitLinkType::Pr);
        assert_eq!(GitLinkType::infer_from_ref("#4567"), GitLinkType::Pr);
        // anything else ⇒ branch
        assert_eq!(
            GitLinkType::infer_from_ref("feat/work-item-tracker"),
            GitLinkType::Branch
        );
        // a short hex token (< 7) is NOT a commit — treated as a branch name.
        assert_eq!(GitLinkType::infer_from_ref("abc"), GitLinkType::Branch);
    }

    #[test]
    fn item_kind_maps_source_to_kind() {
        assert_eq!(FindingSource::BugPrediction.item_kind(), "bug");
        assert_eq!(FindingSource::DocumentedTechDebt.item_kind(), "fixme");
    }
}
