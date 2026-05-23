//! Request-parameter parsing for shadow-ASR-aware filters.
//!
//! Tools accept optional filters — `type_tags`, `effects`,
//! `min_confidence`, `scope_kind`, `signature_shape` — and convert them
//! into SQL WHERE fragments via this helper. Keeps the per-tool filter
//! plumbing uniform.
//!
//! Also exposes [`enclosing_symbol_filter_pass`], which post-filters a
//! generic search-result list against the enclosing symbol's
//! return_type_tags / effects / scope_kind. Each result must serialize
//! to a JSON object containing at least `file_id` and `start_line`.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Post-filter a search-result vector against the enclosing-symbol
/// shadow-ASR criteria. When all filters are None, the input is
/// returned unchanged. Used by Pattern D search tools.
pub async fn enclosing_symbol_filter_pass<R>(
    pool: Option<&PgPool>,
    results: Vec<R>,
    return_type_tags: Option<&[String]>,
    effects: Option<&[String]>,
    scope_kind: Option<&str>,
) -> Vec<R>
where
    R: serde::Serialize,
{
    if return_type_tags.is_none() && effects.is_none() && scope_kind.is_none() {
        return results;
    }
    let Some(pool) = pool else {
        return results;
    };

    let mut keep: Vec<R> = Vec::with_capacity(results.len());
    for r in results {
        let value = match serde_json::to_value(&r) {
            Ok(v) => v,
            Err(_) => {
                keep.push(r);
                continue;
            }
        };
        let file_id = value.get("file_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let start_line = value
            .get("start_line")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        if file_id == 0 {
            keep.push(r);
            continue;
        }
        type SymRow = (i64, String, Vec<String>);
        let sym: Option<SymRow> = sqlx::query_as(
            "SELECT fs.id, fs.kind, COALESCE(fs.return_type_tags, '{}'::text[])
             FROM file_symbols fs
             WHERE fs.file_id = $1
               AND fs.start_line <= $2
               AND fs.end_line >= $2
             ORDER BY (fs.end_line - fs.start_line) ASC
             LIMIT 1",
        )
        .bind(file_id)
        .bind(start_line)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        let Some((symbol_id, kind, rtt)) = sym else {
            continue;
        };
        if let Some(needed) = scope_kind
            && needed != kind
        {
            continue;
        }
        if let Some(needed_tags) = return_type_tags
            && !needed_tags.iter().all(|t| rtt.contains(t))
        {
            continue;
        }
        if let Some(needed_effects) = effects {
            let sym_effects: Vec<String> =
                sqlx::query_scalar("SELECT effect FROM symbol_effects WHERE symbol_id = $1")
                    .bind(symbol_id)
                    .fetch_all(pool)
                    .await
                    .unwrap_or_default();
            if !needed_effects.iter().any(|e| sym_effects.contains(e)) {
                continue;
            }
        }
        keep.push(r);
    }
    keep
}

/// Common shadow-ASR filter parameters. Tools embed this struct via
/// `#[serde(flatten)]` in their request params so all tools share the
/// same parameter names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShadowAsrFilters {
    /// Restrict to symbols whose `return_type_tags` (or
    /// `symbol_parameters.type_tags` when filtering parameters) include
    /// ALL of these tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_tags: Vec<String>,
    /// Restrict to symbols carrying at least one of these effects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
    /// Minimum resolution-confidence on edge-walking tools (0.0-1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_resolution_confidence: Option<f32>,
    /// Restrict to symbols of a given `SymbolKind` (function | class |
    /// trait | …). Pass-through to the existing `kind` column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_kind: Option<String>,
}

impl ShadowAsrFilters {
    pub fn is_empty(&self) -> bool {
        self.type_tags.is_empty()
            && self.effects.is_empty()
            && self.min_resolution_confidence.is_none()
            && self.scope_kind.is_none()
    }

    /// Build a WHERE-clause fragment + bind values for the filters,
    /// keyed off `file_symbols fs`. Returns an empty string when no
    /// filters are set.
    ///
    /// The returned `bind_values` should be appended to the caller's
    /// existing `sqlx::query(...).bind(...)` chain in the same order as
    /// the placeholders appear (`$N`, `$N+1`, ...).
    pub fn to_where_fragment(&self, starting_placeholder: usize) -> (String, Vec<SqlBind>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<SqlBind> = Vec::new();
        let mut idx = starting_placeholder;

        if !self.type_tags.is_empty() {
            clauses.push(format!(
                "COALESCE(fs.return_type_tags, '{{}}'::text[]) @> ${idx}::text[]"
            ));
            binds.push(SqlBind::TextArray(self.type_tags.clone()));
            idx += 1;
        }

        if !self.effects.is_empty() {
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM symbol_effects se WHERE se.symbol_id = fs.id AND se.effect = ANY(${idx}::text[]))"
            ));
            binds.push(SqlBind::TextArray(self.effects.clone()));
            idx += 1;
        }

        if let Some(kind) = &self.scope_kind {
            clauses.push(format!("fs.kind = ${idx}"));
            binds.push(SqlBind::Text(kind.clone()));
            // `idx += 1` would be needed if more filters follow; tracked
            // for shape symmetry but not used past this point in the
            // current set.
            let _ = idx;
        }

        let fragment = if clauses.is_empty() {
            String::new()
        } else {
            format!(" AND {}", clauses.join(" AND "))
        };
        (fragment, binds)
    }
}

/// Type-erased bind value for the WHERE fragment caller. Tools call
/// `bind_to_query` once per bind to attach to their `sqlx::Query`.
#[derive(Debug, Clone)]
pub enum SqlBind {
    Text(String),
    TextArray(Vec<String>),
    Real(f32),
    Int(i64),
}

impl SqlBind {
    /// Attach this bind to a `sqlx::query::Query` (the SQL type returned
    /// by `sqlx::query(...)`). The caller chains:
    ///
    /// ```ignore
    /// let mut q = sqlx::query(&sql);
    /// for b in &binds { q = b.bind_to(q); }
    /// q.execute(pool).await?;
    /// ```
    pub fn bind_to<'q>(
        &'q self,
        q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        match self {
            SqlBind::Text(s) => q.bind(s),
            SqlBind::TextArray(a) => q.bind(a),
            SqlBind::Real(r) => q.bind(r),
            SqlBind::Int(i) => q.bind(i),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filters_produce_empty_fragment() {
        let f = ShadowAsrFilters::default();
        let (frag, binds) = f.to_where_fragment(5);
        assert!(frag.is_empty());
        assert!(binds.is_empty());
        assert!(f.is_empty());
    }

    #[test]
    fn type_tags_filter_produces_array_contains() {
        let f = ShadowAsrFilters {
            type_tags: vec!["result".into(), "owned".into()],
            ..Default::default()
        };
        let (frag, binds) = f.to_where_fragment(2);
        assert!(frag.contains("@>"));
        assert!(frag.contains("$2"));
        assert_eq!(binds.len(), 1);
        matches!(binds[0], SqlBind::TextArray(_));
    }

    #[test]
    fn effects_filter_produces_exists_subquery() {
        let f = ShadowAsrFilters {
            effects: vec!["async".into()],
            ..Default::default()
        };
        let (frag, binds) = f.to_where_fragment(3);
        assert!(frag.contains("symbol_effects"));
        assert!(frag.contains("$3"));
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn combined_filters_chain_with_and() {
        let f = ShadowAsrFilters {
            type_tags: vec!["int".into()],
            effects: vec!["pure".into()],
            scope_kind: Some("function".into()),
            ..Default::default()
        };
        let (frag, binds) = f.to_where_fragment(1);
        assert!(frag.contains(" AND "));
        assert!(frag.contains("$1") && frag.contains("$2") && frag.contains("$3"));
        assert_eq!(binds.len(), 3);
    }

    #[test]
    fn fragment_starts_with_and_for_inline_appending() {
        let f = ShadowAsrFilters {
            type_tags: vec!["int".into()],
            ..Default::default()
        };
        let (frag, _) = f.to_where_fragment(1);
        assert!(frag.starts_with(" AND "));
    }
}
