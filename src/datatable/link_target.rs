//! `LinkTargetType` — the entity kind a data table can be associated with.
//!
//! Backs the generic `data_table_links(table_id, target_type, target_id, role)`
//! bridge (v44), mirroring the `work_item_experiment` bridge. Lets a benchmark /
//! measurement data table be tied to the experiment or work-item it backs, which
//! had no representation before (data tables previously associated only with a
//! `project_id`). Closed vocab per the ADR-003 idiom.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkTargetType {
    Experiment,
    WorkItem,
}

impl LinkTargetType {
    pub const ALL: &'static [LinkTargetType] = &[Self::Experiment, Self::WorkItem];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Experiment => "experiment",
            Self::WorkItem => "work_item",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        Self::ALL
            .iter()
            .map(|x| format!("'{}'", x.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_quotes() {
        for t in LinkTargetType::ALL {
            assert_eq!(LinkTargetType::parse(t.as_str()), Some(*t));
        }
        assert_eq!(LinkTargetType::ALL.len(), 2);
        assert!(LinkTargetType::sql_in_list().contains("'experiment'"));
        assert!(LinkTargetType::sql_in_list().contains("'work_item'"));
        assert_eq!(LinkTargetType::parse("nope"), None);
    }
}
