//! Agent voting over issues / feedback / work-items / experiments.
//!
//! A generic, single-table vote model: `votes(target_type, target_id, agent_id,
//! direction, weight, …)` with a `UNIQUE(target_type, target_id, agent_id)`
//! constraint enforcing **at most one vote per (target, agent)** — the integrity
//! mechanism, since `agent_id` is the client-declared MCP `clientInfo.name`
//! (obtained via `extract_caller`, the same identity primitive the work-item
//! tracker uses for claims). `agent_id` is therefore *identification*, not
//! cryptographic authentication; a `vote_token` seam is left for environments
//! that need stronger guarantees (off by default — see ADR-023).
//!
//! `cast_vote` is an idempotent upsert (re-voting updates direction/weight);
//! `retract_vote` deletes; `tally_votes` aggregates so feedback / work-items can
//! be ranked by support. Vocabularies follow the ADR-003 idiom. Schema: v43
//! (`votes`).

use serde::{Deserialize, Serialize};

fn quoted_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The kind of entity a vote targets. Generic so one table covers every votable
/// entity; extend by adding a variant (and re-applying the CHECK via the v43
/// migration's `install_check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteTargetType {
    WorkItem,
    Feedback,
    Bug,
    Experiment,
}

impl VoteTargetType {
    pub const ALL: &'static [VoteTargetType] =
        &[Self::WorkItem, Self::Feedback, Self::Bug, Self::Experiment];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkItem => "work_item",
            Self::Feedback => "feedback",
            Self::Bug => "bug",
            Self::Experiment => "experiment",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        quoted_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Direction of a vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteDirection {
    Up,
    Down,
}

impl VoteDirection {
    pub const ALL: &'static [VoteDirection] = &[Self::Up, Self::Down];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        quoted_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_type_roundtrips_and_quotes() {
        for t in VoteTargetType::ALL {
            assert_eq!(VoteTargetType::parse(t.as_str()), Some(*t));
        }
        assert_eq!(VoteTargetType::ALL.len(), 4);
        assert!(VoteTargetType::sql_in_list().contains("'work_item'"));
        assert_eq!(VoteTargetType::parse("nope"), None);
    }

    #[test]
    fn direction_roundtrips_and_quotes() {
        for d in VoteDirection::ALL {
            assert_eq!(VoteDirection::parse(d.as_str()), Some(*d));
        }
        assert!(VoteDirection::sql_in_list().contains("'up'"));
        assert!(VoteDirection::sql_in_list().contains("'down'"));
    }
}
