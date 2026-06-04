//! Closed vocabularies for the Phase-4 worktree-coordination state machine (the
//! Rust enforcement of the formally-verified `WorktreeNegotiation` protocol —
//! see `docs/formal/WorktreeNegotiation.{tla,v}`). Per ADR-003 each is a `TEXT`
//! column + a `CHECK` built from a closed Rust enum via a `sql_in_list` helper,
//! with a golden test pinning the set.
//!
//! THE TRUST BOUNDARY (mirrors the TLA⁺/Rocq `GatekeeperSafety` theorem and the
//! v17 CI-evidence gatekeeper): the Editor agent can drive a request to
//! [`CoordinationStatus::Moved`] (a *candidate*), but **only** a git-scanner
//! `stable_restored` [`ProjectEventKind`] event may move it to
//! [`CoordinationStatus::Resolved`]. No agent path reaches `Resolved`.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// State of a worktree-coordination request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationStatus {
    /// Requested; awaiting the editor's response.
    Pending,
    /// The editor accepted the request.
    Accepted,
    /// The editor declined (with a reason); the requester may escalate/withdraw.
    Declined,
    /// The editor reports it moved its edits to a worktree — a CANDIDATE; does
    /// not unblock the dependent on its own.
    Moved,
    /// The git scanner observed the dependency stable again — the gatekeeper
    /// resolution that unblocks the dependent. Agent-unreachable.
    Resolved,
    /// The requester withdrew, or the request expired.
    Cancelled,
}

impl CoordinationStatus {
    pub const ALL: &'static [CoordinationStatus] = &[
        Self::Pending,
        Self::Accepted,
        Self::Declined,
        Self::Moved,
        Self::Resolved,
        Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Moved => "moved",
            Self::Resolved => "resolved",
            Self::Cancelled => "cancelled",
        }
    }

    /// Inverse of [`as_str`](Self::as_str) — a deliberate closed-vocab API member
    /// (ADR-003 idiom) exercised by the golden tests; the live tools map the
    /// user-friendly `accept`/`accepted` aliases by hand, so there is no internal
    /// caller yet (`#[allow(dead_code)]`, like `Severity::rank`).
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// Whether an *agent* (editor/requester) may set this status. `Resolved` is
    /// reserved for the git-scanner gatekeeper — the trust boundary.
    pub fn is_agent_settable(self) -> bool {
        !matches!(self, Self::Resolved)
    }
}

/// SQL `IN (...)` list for the `coordination_requests_status_check` constraint.
pub fn status_sql_in_list() -> String {
    join_quoted(CoordinationStatus::ALL.iter().map(|s| s.as_str()))
}

/// A git-state event the scanner posts for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectEventKind {
    /// The project is back on its stable branch and clean — the gatekeeper
    /// signal that resolves pending coordination requests against it.
    StableRestored,
    /// The project left its stable branch / went dirty.
    WentUnstable,
}

impl ProjectEventKind {
    pub const ALL: &'static [ProjectEventKind] = &[Self::StableRestored, Self::WentUnstable];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StableRestored => "stable_restored",
            Self::WentUnstable => "went_unstable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
}

/// SQL `IN (...)` list for the `project_events_kind_check` constraint.
pub fn event_kind_sql_in_list() -> String {
    join_quoted(ProjectEventKind::ALL.iter().map(|k| k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn coordination_status_vocabulary_is_pinned() {
        let got: HashSet<&str> = CoordinationStatus::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = [
            "pending",
            "accepted",
            "declined",
            "moved",
            "resolved",
            "cancelled",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "CoordinationStatus vocabulary drifted");
        assert_eq!(CoordinationStatus::ALL.len(), 6);
        assert_eq!(got.len(), 6, "duplicate as_str() in CoordinationStatus");
    }

    #[test]
    fn project_event_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = ProjectEventKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = ["stable_restored", "went_unstable"].into_iter().collect();
        assert_eq!(got, expected, "ProjectEventKind vocabulary drifted");
        assert_eq!(ProjectEventKind::ALL.len(), 2);
    }

    #[test]
    fn only_resolved_is_not_agent_settable() {
        // The trust boundary: agents can reach every status except `resolved`.
        for s in CoordinationStatus::ALL {
            assert_eq!(s.is_agent_settable(), *s != CoordinationStatus::Resolved);
        }
    }

    #[test]
    fn parse_roundtrips() {
        for s in CoordinationStatus::ALL {
            assert_eq!(CoordinationStatus::parse(s.as_str()), Some(*s));
        }
        for k in ProjectEventKind::ALL {
            assert_eq!(ProjectEventKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(CoordinationStatus::parse("nope"), None);
        assert_eq!(ProjectEventKind::parse("nope"), None);
    }
}
