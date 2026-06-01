//! Closed vocabulary for the `concurrency_findings` ledger (ADR-003).
//!
//! `finding_kind` is `TEXT` + a `CHECK` built from [`sql_in_list`], with a
//! golden test pinning the set — the same idiom as
//! [`crate::tracker::severity::Severity`] and
//! [`crate::parsing::resolution_kind::ResolutionKind`]. The CHECK is installed
//! by the `v22_concurrency_findings` migration; the `concurrency-scan` cron
//! emits `as_str()` rather than literals so writer ⇄ CHECK stay in lockstep.

use serde::{Deserialize, Serialize};

/// What a `concurrency_findings` row reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyFindingKind {
    /// Shared-memory lock-order cycle (Havender circular wait).
    DeadlockCycle,
    /// A channel sent-to but never received.
    OrphanSend,
    /// A linear receive whose channel has no producer (blocks forever).
    BlockedRecv,
    /// Communication cycle among mutually-blocked processes.
    ChannelCycle,
    /// A lock acquired across many high-centrality call paths (serialization).
    LockContention,
    /// A generic concurrency choke point (e.g. spawn fan-out / channel imbalance).
    Bottleneck,
    /// A blocking-I/O call reachable from an async context on a hot path.
    BlockingInAsyncPath,
}

impl ConcurrencyFindingKind {
    pub const ALL: &'static [ConcurrencyFindingKind] = &[
        Self::DeadlockCycle,
        Self::OrphanSend,
        Self::BlockedRecv,
        Self::ChannelCycle,
        Self::LockContention,
        Self::Bottleneck,
        Self::BlockingInAsyncPath,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeadlockCycle => "deadlock_cycle",
            Self::OrphanSend => "orphan_send",
            Self::BlockedRecv => "blocked_recv",
            Self::ChannelCycle => "channel_cycle",
            Self::LockContention => "lock_contention",
            Self::Bottleneck => "bottleneck",
            Self::BlockingInAsyncPath => "blocking_in_async_path",
        }
    }

    /// Parse from `as_str()` form (closed-vocab surface; exercised by the
    /// roundtrip test). Named `parse` (not `from_str`) to avoid colliding with
    /// the `FromStr` trait method.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// Correctness defects (deadlocks) promote to tracker `kind='bug'`; the rest
    /// are performance/advisory `task`s. Mirrors the `FindingSource::item_kind`
    /// split used by the findings-promotion cron.
    #[allow(dead_code)] // exercised by the unit test; the cron uses FindingSource::item_kind.
    pub fn is_bug(self) -> bool {
        matches!(
            self,
            Self::DeadlockCycle | Self::ChannelCycle | Self::BlockedRecv
        )
    }
}

/// SQL `IN (...)` list for the `chk_concurrency_findings_kind` CHECK — single
/// source of truth shared with the `v22_concurrency_findings` migration.
pub fn sql_in_list() -> String {
    ConcurrencyFindingKind::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn finding_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = ConcurrencyFindingKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect();
        let expected: HashSet<&str> = [
            "deadlock_cycle",
            "orphan_send",
            "blocked_recv",
            "channel_cycle",
            "lock_contention",
            "bottleneck",
            "blocking_in_async_path",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "ConcurrencyFindingKind drifted — update the v22_concurrency_findings CHECK together"
        );
        assert_eq!(ConcurrencyFindingKind::ALL.len(), 7);
    }

    #[test]
    fn parse_roundtrips() {
        for k in ConcurrencyFindingKind::ALL {
            assert_eq!(ConcurrencyFindingKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(ConcurrencyFindingKind::parse("nope"), None);
    }

    #[test]
    fn deadlock_kinds_are_bugs() {
        assert!(ConcurrencyFindingKind::DeadlockCycle.is_bug());
        assert!(ConcurrencyFindingKind::ChannelCycle.is_bug());
        assert!(!ConcurrencyFindingKind::Bottleneck.is_bug());
        assert!(!ConcurrencyFindingKind::OrphanSend.is_bug());
    }
}
