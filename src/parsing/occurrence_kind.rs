//! `OccurrenceKind` — how/where an identifier occurrence appears (ADR-024).
//!
//! Backs `symbol_occurrences.occurrence_kind` (v45). The key distinctions the
//! user asked for: an identifier in CODE (`definition` / `code_reference`) vs in
//! COMMENTARY (`comment` / `doc`) vs in a string literal (`string`). Closed
//! vocab per the ADR-003 idiom.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceKind {
    /// The defining occurrence of a `file_symbols` row.
    Definition,
    /// A use of an identifier in code (the generalization of `symbol_references`).
    CodeReference,
    /// Identifier text inside a (non-doc) comment.
    Comment,
    /// Identifier text inside a string literal.
    String,
    /// Identifier text inside a doc comment / docstring.
    Doc,
}

impl OccurrenceKind {
    pub const ALL: &'static [OccurrenceKind] = &[
        Self::Definition,
        Self::CodeReference,
        Self::Comment,
        Self::String,
        Self::Doc,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::CodeReference => "code_reference",
            Self::Comment => "comment",
            Self::String => "string",
            Self::Doc => "doc",
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
        for k in OccurrenceKind::ALL {
            assert_eq!(OccurrenceKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(OccurrenceKind::ALL.len(), 5);
        assert!(OccurrenceKind::sql_in_list().contains("'code_reference'"));
        assert!(OccurrenceKind::sql_in_list().contains("'doc'"));
        assert_eq!(OccurrenceKind::parse("nope"), None);
    }
}
