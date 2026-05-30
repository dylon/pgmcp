//! Closed vocabularies for the Phase-2 tracker ergonomics layer
//! (`~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`, "Tracker ergonomics
//! & next-action"):
//!
//! - [`SmartView`] — the five fixed agent-facing queues (`my-work`,
//!   `needs-triage`, `overdue`, `blocked`, `next-actionable`). These are
//!   *built-in* semantics over the existing `list_work_items` path, NOT a
//!   persisted `saved_views` table (the set is closed; a new view is a code
//!   change, like a new [`crate::tracker::status::WorkItemStatus`]).
//! - [`BulkOp`] — the operation a `work_item_bulk` call applies to each resolved
//!   target (`set_status` loops through the per-item `set_work_item_status`
//!   chokepoint, so bulk inherits per-item transition legality + auto-unblock).
//!
//! Each mirrors the closed-enum idiom of [`crate::tracker::severity`]: an `ALL`
//! slice (the single source of truth), `as_str` + `parse` (symmetric inverses),
//! and a `#[cfg(test)]` golden-test trio pinning the vocabulary, the round-trip,
//! and the kebab-case wire form. Unlike `severity`, neither vocabulary backs a DB
//! CHECK (these are request-shaping params, not stored columns), so there is no
//! `sql_in_list`.

use serde::{Deserialize, Serialize};

/// A built-in smart-view: a fixed, named queue over the backlog. The kebab-case
/// [`SmartView::as_str`] form is what a tool param carries on the wire
/// (`"my-work"`, `"needs-triage"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SmartView {
    /// Items the caller owns (filter on `work_items.assignee`).
    MyWork,
    /// Reported bugs awaiting user confirmation (`kind='bug' AND status='triage'`).
    NeedsTriage,
    /// Past-due, not-yet-closed items (`due_at < NOW()`, not verified/cancelled/deferred).
    Overdue,
    /// Items currently blocked by an unresolved dependency (`status='blocked'`).
    Blocked,
    /// Workable-now items: actionable status AND no unresolved blocker.
    NextActionable,
}

impl SmartView {
    /// Canonical ordering; also the source of the golden-test vocabulary.
    pub const ALL: &'static [SmartView] = &[
        Self::MyWork,
        Self::NeedsTriage,
        Self::Overdue,
        Self::Blocked,
        Self::NextActionable,
    ];

    /// The kebab-case wire form (matches the `#[serde(rename_all = "kebab-case")]`
    /// derive so the manual and serde paths agree).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MyWork => "my-work",
            Self::NeedsTriage => "needs-triage",
            Self::Overdue => "overdue",
            Self::Blocked => "blocked",
            Self::NextActionable => "next-actionable",
        }
    }

    /// Parse a view from its [`SmartView::as_str`] form; `None` for anything
    /// outside the closed set.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.as_str() == s)
    }
}

/// A bulk operation applied to each item a `work_item_bulk` call resolves. The
/// snake_case [`BulkOp::as_str`] form is the wire value (`"set_status"`,
/// `"reprioritize"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkOp {
    /// Transition each item (through the per-item `set_work_item_status`
    /// chokepoint — so transition legality + the auto-unblock cascade fire per
    /// item).
    SetStatus,
    /// Attach a tag to each item.
    Tag,
    /// Detach a tag from each item.
    Untag,
    /// Set each item's `priority`.
    Reprioritize,
    /// Set (or clear) each item's durable `assignee`.
    Assign,
}

impl BulkOp {
    /// Canonical ordering; also the source of the golden-test vocabulary.
    pub const ALL: &'static [BulkOp] = &[
        Self::SetStatus,
        Self::Tag,
        Self::Untag,
        Self::Reprioritize,
        Self::Assign,
    ];

    /// The snake_case wire form (matches the `#[serde(rename_all = "snake_case")]`
    /// derive).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetStatus => "set_status",
            Self::Tag => "tag",
            Self::Untag => "untag",
            Self::Reprioritize => "reprioritize",
            Self::Assign => "assign",
        }
    }

    /// Parse a bulk op from its [`BulkOp::as_str`] form; `None` for anything
    /// outside the closed set.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|op| op.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn smart_view_vocabulary_is_pinned() {
        let got: HashSet<&str> = SmartView::ALL.iter().map(|v| v.as_str()).collect();
        let expected: HashSet<&str> = [
            "my-work",
            "needs-triage",
            "overdue",
            "blocked",
            "next-actionable",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "SmartView vocabulary drifted from pinned set"
        );
        assert_eq!(SmartView::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() value in SmartView");
    }

    #[test]
    fn bulk_op_vocabulary_is_pinned() {
        let got: HashSet<&str> = BulkOp::ALL.iter().map(|op| op.as_str()).collect();
        let expected: HashSet<&str> = ["set_status", "tag", "untag", "reprioritize", "assign"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "BulkOp vocabulary drifted from pinned set");
        assert_eq!(BulkOp::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() value in BulkOp");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for v in SmartView::ALL {
            assert_eq!(SmartView::parse(v.as_str()), Some(*v));
        }
        assert_eq!(SmartView::parse("nonsense"), None);
        for op in BulkOp::ALL {
            assert_eq!(BulkOp::parse(op.as_str()), Some(*op));
        }
        assert_eq!(BulkOp::parse("nonsense"), None);
    }

    #[test]
    fn wire_forms_are_well_formed() {
        // SmartView is kebab-case (no underscores); BulkOp is snake_case (no
        // dashes). Pin both so a rename can't silently cross the two styles.
        for v in SmartView::ALL {
            let s = v.as_str();
            assert!(!s.contains('_'), "SmartView '{s}' must be kebab-case");
            assert!(!s.is_empty());
        }
        for op in BulkOp::ALL {
            let s = op.as_str();
            assert!(!s.contains('-'), "BulkOp '{s}' must be snake_case");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn serde_matches_as_str() {
        // The serde rename derives must agree with the manual as_str forms so
        // the two (de)serialization paths can't diverge.
        for v in SmartView::ALL {
            let json = serde_json::to_string(v).expect("serialize SmartView");
            assert_eq!(json, format!("\"{}\"", v.as_str()));
        }
        for op in BulkOp::ALL {
            let json = serde_json::to_string(op).expect("serialize BulkOp");
            assert_eq!(json, format!("\"{}\"", op.as_str()));
        }
    }
}
