//! Closed vocabularies for the safe row-filter / sort representation.
//!
//! These enums are *request-shaping* (no DB CHECK), but they are closed sets
//! with golden tests because the SQL compiler in
//! [`crate::db::queries::data_tables`] renders [`FilterOp::sql_cmp`],
//! [`SortDir::as_sql`], and [`Combinator::as_sql`] as **literal** SQL fragments.
//! Rendering only from these finite, audited sets (never from caller text) is
//! what keeps dynamic filter/sort SQL injection-inert; all operands and JSON
//! field keys are bound parameters.

use serde::{Deserialize, Serialize};

/// A predicate operator over a JSON field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    /// Field equals value (JSONB containment `data @> {field:value}`).
    Eq,
    /// Field does not equal value.
    Ne,
    /// Field > value (typed comparison; needs an ordered/typed column).
    Gt,
    /// Field < value.
    Lt,
    /// Field >= value.
    Gte,
    /// Field <= value.
    Lte,
    /// Field's text contains the (string) value (case-insensitive substring).
    Contains,
    /// Field key is present in the row object.
    Exists,
}

impl FilterOp {
    pub const ALL: &'static [FilterOp] = &[
        Self::Eq,
        Self::Ne,
        Self::Gt,
        Self::Lt,
        Self::Gte,
        Self::Lte,
        Self::Contains,
        Self::Exists,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Gt => "gt",
            Self::Lt => "lt",
            Self::Gte => "gte",
            Self::Lte => "lte",
            Self::Contains => "contains",
            Self::Exists => "exists",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|op| op.as_str() == s)
    }

    /// The literal SQL comparison operator for the ordered ops. `None` for the
    /// non-relational ops (eq/ne/contains/exists), which compile differently.
    pub fn sql_cmp(self) -> Option<&'static str> {
        match self {
            Self::Gt => Some(">"),
            Self::Lt => Some("<"),
            Self::Gte => Some(">="),
            Self::Lte => Some("<="),
            _ => None,
        }
    }

    /// Whether this op requires a `value` operand. `exists` is the only op that
    /// takes none (it tests key presence).
    pub fn needs_value(self) -> bool {
        !matches!(self, Self::Exists)
    }
}

/// Sort direction. Cannot be a bound parameter (a direction is not a value), so
/// it is rendered as one of two literals via [`Self::as_sql`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "asc" | "ascending" => Some(Self::Asc),
            "desc" | "descending" => Some(Self::Desc),
            _ => None,
        }
    }

    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// How multiple predicates combine. Rendered as the literal `AND` / `OR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Combinator {
    /// All predicates must hold (`AND`).
    All,
    /// Any predicate may hold (`OR`).
    Any,
}

impl Combinator {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "and" => Some(Self::All),
            "any" | "or" => Some(Self::Any),
            _ => None,
        }
    }

    pub fn as_sql(self) -> &'static str {
        match self {
            Self::All => "AND",
            Self::Any => "OR",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn filter_op_vocabulary_is_pinned() {
        let got: HashSet<&str> = FilterOp::ALL.iter().map(|o| o.as_str()).collect();
        let expected: HashSet<&str> = ["eq", "ne", "gt", "lt", "gte", "lte", "contains", "exists"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "FilterOp vocabulary drifted");
        assert_eq!(FilterOp::ALL.len(), 8);
        assert_eq!(got.len(), 8, "duplicate as_str() value in FilterOp");
    }

    #[test]
    fn parse_roundtrips() {
        for o in FilterOp::ALL {
            assert_eq!(FilterOp::parse(o.as_str()), Some(*o));
        }
        assert_eq!(FilterOp::parse("like"), None);
        assert_eq!(SortDir::parse("ASC"), Some(SortDir::Asc));
        assert_eq!(SortDir::parse("Descending"), Some(SortDir::Desc));
        assert_eq!(SortDir::parse("sideways"), None);
        assert_eq!(Combinator::parse("AND"), Some(Combinator::All));
        assert_eq!(Combinator::parse("or"), Some(Combinator::Any));
        assert_eq!(Combinator::parse("xor"), None);
    }

    #[test]
    fn ordered_ops_have_a_cmp_and_others_do_not() {
        for o in FilterOp::ALL {
            let ordered = matches!(
                o,
                FilterOp::Gt | FilterOp::Lt | FilterOp::Gte | FilterOp::Lte
            );
            assert_eq!(ordered, o.sql_cmp().is_some(), "{o:?} mismatch");
        }
        assert_eq!(FilterOp::Gt.sql_cmp(), Some(">"));
        assert_eq!(FilterOp::Lte.sql_cmp(), Some("<="));
        assert_eq!(FilterOp::Eq.sql_cmp(), None);
    }

    #[test]
    fn only_exists_needs_no_value() {
        for o in FilterOp::ALL {
            assert_eq!(o.needs_value(), *o != FilterOp::Exists);
        }
    }

    #[test]
    fn direction_and_combinator_render_as_literals() {
        assert_eq!(SortDir::Asc.as_sql(), "ASC");
        assert_eq!(SortDir::Desc.as_sql(), "DESC");
        assert_eq!(Combinator::All.as_sql(), "AND");
        assert_eq!(Combinator::Any.as_sql(), "OR");
    }
}
