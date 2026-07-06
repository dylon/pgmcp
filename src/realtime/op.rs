//! Closed realtime-event `op` vocabulary. Per ADR-003 the DB CHECK on
//! `pgmcp_realtime_events.op` is built from [`sql_in_list`] — the single
//! source of truth shared with the `pgmcp_realtime_events_op_check` constraint
//! installed by [`crate::db::migrations::v64_realtime_events`]. Mirrors the
//! [`crate::realtime::topic`] idiom.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// The mutation shape an event describes. A consumer replaying the log applies
/// each op to its local projection of the entity. The ordering here is the
/// exact left-to-right order of the `op IN (...)` CHECK in migration v64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Create-or-update of a keyed entity (idempotent replace).
    Upsert,
    /// Removal / retirement of a keyed entity.
    Delete,
    /// A point-in-time pulse with no durable entity delta (cron run, control
    /// action).
    Tick,
    /// A new immutable item appended to an append-only stream.
    Append,
    /// A whole-of-entity snapshot (rollup counters / resource sample).
    Snapshot,
}

impl Op {
    /// Canonical ordering; also the source of the DB CHECK vocabulary. MUST
    /// match the `op IN (...)` order in migration v64.
    pub const ALL: &'static [Op] = &[
        Self::Upsert,
        Self::Delete,
        Self::Tick,
        Self::Append,
        Self::Snapshot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
            Self::Tick => "tick",
            Self::Append => "append",
            Self::Snapshot => "snapshot",
        }
    }

    /// Inverse of [`Op::as_str`]. Part of the closed-vocab surface with no
    /// in-crate caller yet (same `#[allow(dead_code)]` idiom as
    /// [`crate::tracker::severity::Severity::rank`]); exercised by the tests.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|o| o.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`Op::ALL`] — the single source of
/// truth shared with the `pgmcp_realtime_events_op_check` constraint (v64).
pub fn sql_in_list() -> String {
    join_quoted(Op::ALL.iter().map(|o| o.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn op_vocabulary_is_pinned() {
        let got: HashSet<&str> = Op::ALL.iter().map(|o| o.as_str()).collect();
        let expected: HashSet<&str> = ["upsert", "delete", "tick", "append", "snapshot"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "Op vocabulary drifted from pinned set");
        assert_eq!(Op::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() value in Op");
    }

    /// Golden test: `sql_in_list` must reproduce the exact `op IN (...)`
    /// literal in `src/db/migrations/v64_realtime_events.rs`.
    #[test]
    fn sql_in_list_matches_v64_literal() {
        assert_eq!(
            format!("op IN ({})", sql_in_list()),
            "op IN ('upsert','delete','tick','append','snapshot')"
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for o in Op::ALL {
            assert_eq!(Op::parse(o.as_str()), Some(*o));
        }
        assert_eq!(Op::parse("bogus"), None);
    }
}
