//! Closed work-item `status` lifecycle vocabulary. The legal transitions
//! between statuses (and which actor may perform each) live in
//! [`crate::tracker::transition`]. As with [`crate::tracker::kind`], the DB
//! CHECK on `work_items.status` is built from [`sql_in_list`].

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    /// Created, not yet actionable (e.g. unmet dependencies / not groomed).
    Pending,
    /// A reported bug awaiting triage; not yet actionable. Agents may report
    /// (move an item here) and propose a severity, but only a token-bearing
    /// user may confirm it real via `work_item_triage`. Counts as open for the
    /// completion roll-up (an untriaged bug dilutes its parent).
    Triage,
    /// A bug that has been triaged and accepted — reproduced, severity set,
    /// ready to be worked. Reached only via the user-token-gated
    /// `work_item_triage` tool. Counts as open for the completion roll-up.
    Confirmed,
    /// All `depends_on` satisfied; eligible to start.
    Ready,
    /// Actively being worked.
    InProgress,
    /// Was actionable but a `blocks` edge or external blocker fired.
    Blocked,
    /// An **agent** asserts completion. Explicitly **not** trusted; awaits
    /// evidence. Never counts as "done" for roll-up.
    ClaimedDone,
    /// Evidence collection in flight (a CI run / Stop-hook gate executing).
    Verifying,
    /// Machine-checkable acceptance criteria all passed with valid evidence.
    /// The ONLY status counted as "done" by the completion roll-up.
    Verified,
    /// Verification ran and failed (evidence verdict = fail / auditor block).
    Rejected,
    /// Explicitly skipped by the **user** via a `scope_negotiations` row.
    /// Excluded from completion denominators. Agents cannot reach this state.
    Deferred,
    /// Abandoned; excluded from completion denominators.
    Cancelled,
}

impl WorkItemStatus {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [WorkItemStatus] = &[
        Self::Pending,
        Self::Triage,
        Self::Confirmed,
        Self::Ready,
        Self::InProgress,
        Self::Blocked,
        Self::ClaimedDone,
        Self::Verifying,
        Self::Verified,
        Self::Rejected,
        Self::Deferred,
        Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Triage => "triage",
            Self::Confirmed => "confirmed",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::ClaimedDone => "claimed_done",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::Deferred => "deferred",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|st| st.as_str() == s)
    }

    /// The single "done" status for verified-completion roll-up.
    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }

    /// Statuses excluded from completion numerator AND denominator (a
    /// user-deferred or cancelled subtree neither helps nor hurts).
    pub fn is_excluded_from_rollup(self) -> bool {
        matches!(self, Self::Deferred | Self::Cancelled)
    }
}

/// SQL `IN (...)` value list built from [`WorkItemStatus::ALL`] — the single
/// source of truth shared with the `work_items_status_check` constraint.
pub fn sql_in_list() -> String {
    join_quoted(WorkItemStatus::ALL.iter().map(|s| s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn status_vocabulary_is_pinned() {
        let got: HashSet<&str> = WorkItemStatus::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = [
            "pending",
            "triage",
            "confirmed",
            "ready",
            "in_progress",
            "blocked",
            "claimed_done",
            "verifying",
            "verified",
            "rejected",
            "deferred",
            "cancelled",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "WorkItemStatus vocabulary drifted from pinned set"
        );
        assert_eq!(WorkItemStatus::ALL.len(), 12);
        assert_eq!(got.len(), 12, "duplicate as_str() value in WorkItemStatus");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for s in WorkItemStatus::ALL {
            assert_eq!(WorkItemStatus::parse(s.as_str()), Some(*s));
        }
        assert_eq!(WorkItemStatus::parse("done"), None);
    }

    #[test]
    fn only_verified_is_done() {
        for s in WorkItemStatus::ALL {
            assert_eq!(s.is_verified(), *s == WorkItemStatus::Verified);
        }
    }
}
