//! Project-level dependency graph (Phase 4): the closed vocabularies for the
//! `project_dependencies` edge, plus (in submodules) manifest parsing, import
//! inference, and the `project_dependents` / `project_dependencies` queries.
//!
//! Per ADR-003 each vocabulary is a `TEXT` column + a `CHECK` built from a
//! closed Rust enum via a `sql_in_list` helper, with a golden test pinning the
//! set — the same idiom as [`crate::tracker::severity`] and
//! [`crate::a2a::mailbox`].

pub mod coord_store;
pub mod coordination;
pub mod gitstate;
pub mod manifest;
pub mod store;

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// How a dependency is wired in the manifest. Stored in `project_dependencies.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    /// A local `path = "../other"` dependency (highest-precision project link).
    Path,
    /// A `git = "…"` dependency.
    Git,
    /// A registry (crates.io / npm / PyPI) dependency.
    Registry,
}

impl DepKind {
    pub const ALL: &'static [DepKind] = &[Self::Path, Self::Git, Self::Registry];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Git => "git",
            Self::Registry => "registry",
        }
    }

    /// Inverse of [`as_str`](Self::as_str). A deliberate closed-vocab API member
    /// (the ADR-003 idiom) exercised by the golden tests; `#[allow(dead_code)]`
    /// documents it has no internal caller yet, like `Severity::rank`.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` list for the `project_dependencies_kind_check` constraint.
pub fn dep_kind_sql_in_list() -> String {
    join_quoted(DepKind::ALL.iter().map(|k| k.as_str()))
}

/// Where a project→project dependency edge came from. Stored in
/// `project_dependencies.source`. Manifest edges are highest-confidence;
/// import-inferred are lower; `asserted` is recorded when an agent's compile
/// failure names a broken dependency (the robust reactive path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepSource {
    /// Parsed from a Cargo.toml / package.json / etc. manifest.
    Cargo,
    /// Inferred from cross-project symbol/import references.
    Import,
    /// Declared in `.pgmcp.toml` or via a tool.
    Manual,
    /// Asserted by an agent whose build failed naming the dependency.
    Asserted,
}

impl DepSource {
    pub const ALL: &'static [DepSource] =
        &[Self::Cargo, Self::Import, Self::Manual, Self::Asserted];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Import => "import",
            Self::Manual => "manual",
            Self::Asserted => "asserted",
        }
    }

    /// Inverse of [`as_str`](Self::as_str) — a deliberate closed-vocab API member
    /// (ADR-003 idiom) exercised by the golden tests; no internal caller yet.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s2| s2.as_str() == s)
    }
}

/// SQL `IN (...)` list for the `project_dependencies_source_check` constraint.
pub fn dep_source_sql_in_list() -> String {
    join_quoted(DepSource::ALL.iter().map(|s| s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn dep_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = DepKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = ["path", "git", "registry"].into_iter().collect();
        assert_eq!(got, expected, "DepKind vocabulary drifted");
        assert_eq!(DepKind::ALL.len(), 3);
        assert_eq!(got.len(), 3, "duplicate as_str() in DepKind");
    }

    #[test]
    fn dep_source_vocabulary_is_pinned() {
        let got: HashSet<&str> = DepSource::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = ["cargo", "import", "manual", "asserted"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "DepSource vocabulary drifted");
        assert_eq!(DepSource::ALL.len(), 4);
        assert_eq!(got.len(), 4, "duplicate as_str() in DepSource");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in DepKind::ALL {
            assert_eq!(DepKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(DepKind::parse("nope"), None);
        for s in DepSource::ALL {
            assert_eq!(DepSource::parse(s.as_str()), Some(*s));
        }
        assert_eq!(DepSource::parse("nope"), None);
    }

    #[test]
    fn sql_in_lists_quote_every_value() {
        let k = dep_kind_sql_in_list();
        assert!(k.contains("'path'"), "got: {k}");
        assert_eq!(k.matches('\'').count(), DepKind::ALL.len() * 2);
        let s = dep_source_sql_in_list();
        assert!(s.contains("'asserted'"), "got: {s}");
        assert_eq!(s.matches('\'').count(), DepSource::ALL.len() * 2);
    }
}
