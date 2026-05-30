//! The closed `ColumnType` vocabulary for a data table's optional typed-column
//! schema.
//!
//! Per ADR-003 (the same idiom as [`crate::tracker::severity::Severity`]) this
//! is a `TEXT` column (`data_table_columns.data_type`) plus a `CHECK` built from
//! the closed Rust enum via [`sql_in_list`], with a `#[cfg(test)]` golden test
//! pinning the vocabulary. The enum is the single source of truth shared by the
//! DB CHECK, the row validator ([`crate::datatable::validate`]), and the
//! type-aware filter/sort/aggregate SQL casts.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// The type a declared column accepts. `Json` is the in-schema escape hatch for
/// arbitrary nested values; `Number` is `f64` (accepts integers too), `Integer`
/// is a whole number, `Timestamp` is an RFC3339 string or epoch-seconds number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    Text,
    Integer,
    Number,
    Boolean,
    Timestamp,
    Json,
}

impl ColumnType {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [ColumnType] = &[
        Self::Text,
        Self::Integer,
        Self::Number,
        Self::Boolean,
        Self::Timestamp,
        Self::Json,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Json => "json",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }

    /// Whether numeric aggregation (sum/avg/stddev/median, numeric min/max) is
    /// meaningful for this type. Only `Integer` / `Number` qualify; `min`/`max`
    /// over `Text` / `Timestamp` are still allowed but compare lexically /
    /// temporally (see [`Self::sql_cast`]).
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Number)
    }

    /// The SQL cast applied to a `data->>'field'` text extraction for ordered
    /// comparison / sorting / aggregation. `None` ⇒ compare as text (lexical).
    /// Chosen in Rust from the *declared* type — never from caller input — so
    /// the only non-bound SQL fragment is one of a finite, audited set.
    pub fn sql_cast(self) -> Option<&'static str> {
        match self {
            Self::Integer | Self::Number => Some("numeric"),
            Self::Timestamp => Some("timestamptz"),
            Self::Text | Self::Boolean | Self::Json => None,
        }
    }
}

/// SQL `IN (...)` value list built from [`ColumnType::ALL`] — the single source
/// of truth shared with the `data_table_columns_type_check` constraint.
pub fn sql_in_list() -> String {
    join_quoted(ColumnType::ALL.iter().map(|t| t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn vocabulary_is_pinned() {
        let got: HashSet<&str> = ColumnType::ALL.iter().map(|t| t.as_str()).collect();
        let expected: HashSet<&str> = ["text", "integer", "number", "boolean", "timestamp", "json"]
            .into_iter()
            .collect();
        assert_eq!(
            got, expected,
            "ColumnType vocabulary drifted from pinned set"
        );
        assert_eq!(ColumnType::ALL.len(), 6);
        assert_eq!(got.len(), 6, "duplicate as_str() value in ColumnType");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for t in ColumnType::ALL {
            assert_eq!(ColumnType::parse(t.as_str()), Some(*t));
        }
        assert_eq!(ColumnType::parse("decimal"), None);
        assert_eq!(ColumnType::parse(""), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.contains("'integer'"), "got: {s}");
        assert!(s.contains("'timestamp'"));
        assert!(s.contains("'json'"));
        assert_eq!(s.matches('\'').count(), ColumnType::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), ColumnType::ALL.len() - 1);
    }

    #[test]
    fn numeric_and_casts_are_consistent() {
        assert!(ColumnType::Integer.is_numeric());
        assert!(ColumnType::Number.is_numeric());
        assert!(!ColumnType::Text.is_numeric());
        assert_eq!(ColumnType::Number.sql_cast(), Some("numeric"));
        assert_eq!(ColumnType::Timestamp.sql_cast(), Some("timestamptz"));
        assert_eq!(ColumnType::Text.sql_cast(), None);
        assert_eq!(ColumnType::Json.sql_cast(), None);
    }
}
