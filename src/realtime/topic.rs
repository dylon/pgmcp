//! Closed realtime-event `topic` vocabulary. Per ADR-003 the DB CHECK on
//! `pgmcp_realtime_events.topic` is built from [`sql_in_list`] — the single
//! source of truth shared with the `pgmcp_realtime_events_topic_check`
//! constraint installed by
//! [`crate::db::migrations::v64_realtime_events`]. The same idiom as
//! [`crate::tracker::status`] / [`crate::tracker::severity`]: a closed Rust
//! enum + `as_str`/`parse`/`sql_in_list`, with a `#[cfg(test)]` golden test
//! pinning the vocabulary (and its ordering) against the v64 literal.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// Which control-plane stream an event belongs to. One variant per web-UI
/// pane / live view; consumers subscribe by topic. The ordering here is
/// load-bearing: it is the exact left-to-right order of the `topic IN (...)`
/// CHECK in migration v64, so `sql_in_list` reproduces that literal verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Work-item / bug tracker status transitions.
    Tracker,
    /// Session + durable mandate upserts / retirements.
    Mandate,
    /// Cron run ticks (one per persisted `cron_run_history` row).
    Cron,
    /// A2A task-state transitions.
    Task,
    /// Indexer batch-commit rollups (workspace rescan / initial scan).
    Index,
    /// MCP-client connect / activity / disconnect.
    Client,
    /// External-scanner findings ingest.
    Scanner,
    /// Fleet-wide control-plane actions (all-stop halt / resume).
    Control,
    /// Crucible trace span open / close (root spans only).
    Trace,
    /// Periodic resource-usage snapshots (RSS / CPU / memory).
    Status,
}

impl Topic {
    /// Canonical ordering; also the source of the DB CHECK vocabulary. MUST
    /// match the `topic IN (...)` order in migration v64.
    pub const ALL: &'static [Topic] = &[
        Self::Tracker,
        Self::Mandate,
        Self::Cron,
        Self::Task,
        Self::Index,
        Self::Client,
        Self::Scanner,
        Self::Control,
        Self::Trace,
        Self::Status,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracker => "tracker",
            Self::Mandate => "mandate",
            Self::Cron => "cron",
            Self::Task => "task",
            Self::Index => "index",
            Self::Client => "client",
            Self::Scanner => "scanner",
            Self::Control => "control",
            Self::Trace => "trace",
            Self::Status => "status",
        }
    }

    /// Inverse of [`Topic::as_str`]. A deliberate member of the closed-vocab
    /// surface (the consumer side may parse a stored `topic` back into the
    /// enum) with no in-crate caller yet — the same `#[allow(dead_code)]` idiom
    /// as [`crate::tracker::severity::Severity::rank`]; exercised by the tests.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`Topic::ALL`] — the single source of
/// truth shared with the `pgmcp_realtime_events_topic_check` constraint (v64).
pub fn sql_in_list() -> String {
    join_quoted(Topic::ALL.iter().map(|t| t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn topic_vocabulary_is_pinned() {
        let got: HashSet<&str> = Topic::ALL.iter().map(|t| t.as_str()).collect();
        let expected: HashSet<&str> = [
            "tracker", "mandate", "cron", "task", "index", "client", "scanner", "control", "trace",
            "status",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "Topic vocabulary drifted from pinned set");
        assert_eq!(Topic::ALL.len(), 10);
        assert_eq!(got.len(), 10, "duplicate as_str() value in Topic");
    }

    /// Golden test: `sql_in_list` must reproduce the exact `topic IN (...)`
    /// literal in `src/db/migrations/v64_realtime_events.rs`. v64 builds its
    /// CHECK from this same function, so this pins both to one literal.
    #[test]
    fn sql_in_list_matches_v64_literal() {
        assert_eq!(
            format!("topic IN ({})", sql_in_list()),
            "topic IN ('tracker','mandate','cron','task','index','client','scanner','control','trace','status')"
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for t in Topic::ALL {
            assert_eq!(Topic::parse(t.as_str()), Some(*t));
        }
        assert_eq!(Topic::parse("bogus"), None);
    }
}
