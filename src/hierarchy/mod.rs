//! Hierarchical inter+intra-project intelligence (ADR-027, item 15).
//!
//! The containment chain `symbol ⊳ function ⊳ file ⊳ module ⊳ project ⊳ group ⊳
//! workspace` is the spine that both the metric-rollup engine (`rollup`) and the
//! category subsystem (`crate::category`) act over. This module holds the closed
//! vocabularies for project grouping (`GroupKind` / `GroupRole`) and the level
//! ladder (`HierLevel`), plus the dependency-graph + rollup engines.
//!
//! Vocabularies follow the ADR-003 idiom (enum + `ALL` + `as_str` + `parse` +
//! `sql_in_list` + golden parity test); each backs a TEXT + CHECK column.

#![allow(dead_code)]

pub mod grouping;
pub mod rollup;

use serde::{Deserialize, Serialize};

/// How a `project_groups` row was formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    /// Git worktrees of one repository (same common-dir + root commits).
    WorktreeFamily,
    /// Multiple package manifests under one repository root.
    Monorepo,
    /// Declared in a project's `.pgmcp.toml [group]`.
    Declared,
    /// Operator-created via the grouping tools.
    Manual,
}

impl GroupKind {
    pub const ALL: &'static [GroupKind] = &[
        Self::WorktreeFamily,
        Self::Monorepo,
        Self::Declared,
        Self::Manual,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorktreeFamily => "worktree_family",
            Self::Monorepo => "monorepo",
            Self::Declared => "declared",
            Self::Manual => "manual",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
    pub fn sql_in_list() -> String {
        sql_in_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// A project's role within a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRole {
    /// The canonical project of the group (e.g. the worktree-family main).
    Main,
    /// A non-main member.
    Member,
}

impl GroupRole {
    pub const ALL: &'static [GroupRole] = &[Self::Main, Self::Member];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Member => "member",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
    pub fn sql_in_list() -> String {
        sql_in_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// A level of the containment hierarchy — the discriminator on every rollup
/// metric row (`module_metrics` / `project_metrics` / `hier_group_metrics`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HierLevel {
    Symbol,
    Function,
    File,
    Module,
    Project,
    Group,
    Workspace,
}

impl HierLevel {
    pub const ALL: &'static [HierLevel] = &[
        Self::Symbol,
        Self::Function,
        Self::File,
        Self::Module,
        Self::Project,
        Self::Group,
        Self::Workspace,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Function => "function",
            Self::File => "file",
            Self::Module => "module",
            Self::Project => "project",
            Self::Group => "group",
            Self::Workspace => "workspace",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
    pub fn sql_in_list() -> String {
        sql_in_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

fn sql_in_list<'a>(it: impl Iterator<Item = &'a str>) -> String {
    it.map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_kind_roundtrips() {
        for k in GroupKind::ALL {
            assert_eq!(GroupKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(GroupKind::ALL.len(), 4);
        assert!(GroupKind::sql_in_list().contains("'worktree_family'"));
        assert_eq!(GroupKind::parse("nope"), None);
    }

    #[test]
    fn group_role_roundtrips() {
        for r in GroupRole::ALL {
            assert_eq!(GroupRole::parse(r.as_str()), Some(*r));
        }
        assert_eq!(GroupRole::ALL.len(), 2);
        assert!(GroupRole::sql_in_list().contains("'main'"));
    }

    #[test]
    fn hier_level_roundtrips() {
        for l in HierLevel::ALL {
            assert_eq!(HierLevel::parse(l.as_str()), Some(*l));
        }
        assert_eq!(HierLevel::ALL.len(), 7);
        assert!(HierLevel::sql_in_list().contains("'workspace'"));
        assert!(HierLevel::sql_in_list().contains("'module'"));
    }
}
