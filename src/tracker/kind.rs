//! Closed work-item `kind` taxonomy — a single mutually-exclusive
//! classification per item. Per ADR-003, a closed/evolvable-but-known
//! vocabulary is modeled as `TEXT` + `CHECK` + a closed Rust enum (the
//! `MandatePolarity` idiom in `crate::sessions::polarity`). The DB CHECK on
//! `work_items.kind` is built from [`sql_in_list`] in
//! `crate::db::migrations::v4_work_items`, so this enum is the single source
//! of truth and a `#[cfg(test)]` golden test pins the vocabulary.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    /// Top-level container for a body of work.
    Plan,
    /// A desired outcome (decomposes into epics).
    Goal,
    /// A large unit of work under a goal (decomposes into tasks).
    Epic,
    /// A unit of work (decomposes into sub-tasks).
    Task,
    /// A child task at arbitrary depth.
    SubTask,
    /// A small actionable item.
    Todo,
    /// A defect to fix.
    Fixme,
    /// A suggestion / possibility.
    Idea,
    /// A brainstorming session: a container that groups loosely-captured
    /// `idea` children for later triage / promotion to tasks.
    Brainstorm,
    /// A free-form note.
    Note,
    /// A follow-up question.
    Question,
    /// An optional enhancement.
    NiceToHave,
    /// A discrete action item.
    ActionItem,
    /// A tracked scientific experiment (linked to the experiment subsystem via
    /// the Phase-10 `work_item_experiment` bridge).
    Experiment,
}

impl WorkItemKind {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [WorkItemKind] = &[
        Self::Plan,
        Self::Goal,
        Self::Epic,
        Self::Task,
        Self::SubTask,
        Self::Todo,
        Self::Fixme,
        Self::Idea,
        Self::Brainstorm,
        Self::Note,
        Self::Question,
        Self::NiceToHave,
        Self::ActionItem,
        Self::Experiment,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Goal => "goal",
            Self::Epic => "epic",
            Self::Task => "task",
            Self::SubTask => "sub_task",
            Self::Todo => "todo",
            Self::Fixme => "fixme",
            Self::Idea => "idea",
            Self::Brainstorm => "brainstorm",
            Self::Note => "note",
            Self::Question => "question",
            Self::NiceToHave => "nice_to_have",
            Self::ActionItem => "action_item",
            Self::Experiment => "experiment",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` value list (e.g. `'plan','goal',...`) built from
/// [`WorkItemKind::ALL`] — the single source of truth shared with the
/// `work_items_kind_check` migration constraint.
pub fn sql_in_list() -> String {
    join_quoted(WorkItemKind::ALL.iter().map(|k| k.as_str()))
}

/// Single-quote each value and comma-join — shared SQL `IN`-list builder.
pub(crate) fn join_quoted<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = WorkItemKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = [
            "plan",
            "goal",
            "epic",
            "task",
            "sub_task",
            "todo",
            "fixme",
            "idea",
            "brainstorm",
            "note",
            "question",
            "nice_to_have",
            "action_item",
            "experiment",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "WorkItemKind vocabulary drifted from pinned set"
        );
        assert_eq!(WorkItemKind::ALL.len(), 14);
        assert_eq!(got.len(), 14, "duplicate as_str() value in WorkItemKind");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in WorkItemKind::ALL {
            assert_eq!(WorkItemKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(WorkItemKind::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.starts_with("'plan'"), "got: {s}");
        assert!(s.contains("'experiment'"));
        // Two quotes per value, no trailing/leading comma issues.
        assert_eq!(s.matches('\'').count(), WorkItemKind::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), WorkItemKind::ALL.len() - 1);
    }
}
