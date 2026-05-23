//! Memory ↔ symbol anchoring facets.
//!
//! When a `memory_anchor_entity` call links a memory node to a
//! `file_symbols` row, this module computes and persists the
//! derived shadow-ASR facets (scope_path, effect set, return type
//! tags). Pattern I in the unified-semantic-representation plan.
//!
//! The facets are stored as a JSONB column on memory relations so the
//! existing memory_relations storage doesn't need new columns; queries
//! that need to filter (e.g. "forget memories anchored to deprecated
//! symbols") use JSONB path queries.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Derived facets snapshot at anchor time. Stored as JSONB on the
/// memory relation row that links the memory entity to the symbol.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolFacets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_depth: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_type_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
}

impl SymbolFacets {
    pub fn is_empty(&self) -> bool {
        self.scope_path.is_none()
            && self.kind.is_none()
            && self.return_type_tags.is_empty()
            && self.effects.is_empty()
    }
}

/// Compute the facet snapshot for a given symbol_id. Returns
/// `SymbolFacets::default()` (empty) when the symbol doesn't exist or
/// has no shadow-ASR data populated yet.
pub async fn facets_for_symbol(pool: &PgPool, symbol_id: i64) -> Result<SymbolFacets, sqlx::Error> {
    type Row = (Option<String>, Option<i32>, String, Vec<String>);
    let opt: Option<Row> = sqlx::query_as(
        "SELECT scope_path, scope_depth, kind,
                COALESCE(return_type_tags, '{}'::text[])
         FROM file_symbols
         WHERE id = $1",
    )
    .bind(symbol_id)
    .fetch_optional(pool)
    .await?;
    let Some((scope_path, scope_depth, kind, return_type_tags)) = opt else {
        return Ok(SymbolFacets::default());
    };
    let effects: Vec<String> = sqlx::query_scalar(
        "SELECT effect FROM symbol_effects WHERE symbol_id = $1 ORDER BY effect",
    )
    .bind(symbol_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    Ok(SymbolFacets {
        scope_path,
        scope_depth,
        kind: Some(kind),
        return_type_tags,
        effects,
    })
}

/// Serialize a facet snapshot to JSONB for storage on a memory relation.
pub fn facets_to_json(facets: &SymbolFacets) -> serde_json::Value {
    serde_json::to_value(facets).unwrap_or(serde_json::Value::Null)
}

/// Parse a facet snapshot back from JSONB. Returns the default (empty)
/// snapshot when the JSON doesn't match the schema.
pub fn facets_from_json(value: &serde_json::Value) -> SymbolFacets {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Filter for memory queries that want to constrain on the anchored
/// symbol's facets. All fields are optional; specifying multiple is
/// an AND.
#[derive(Debug, Clone, Default)]
pub struct MemoryFacetFilter {
    pub require_effects: Vec<String>,
    pub require_kind: Option<String>,
    pub scope_path_prefix: Option<String>,
}

impl MemoryFacetFilter {
    pub fn matches(&self, facets: &SymbolFacets) -> bool {
        if !self
            .require_effects
            .iter()
            .all(|e| facets.effects.iter().any(|x| x == e))
        {
            return false;
        }
        if let Some(kind) = &self.require_kind
            && facets.kind.as_deref() != Some(kind.as_str())
        {
            return false;
        }
        if let Some(prefix) = &self.scope_path_prefix {
            match facets.scope_path.as_deref() {
                Some(path) if path.starts_with(prefix) => {}
                _ => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_facets() {
        let facets = SymbolFacets {
            scope_path: Some("crate::auth::login".into()),
            scope_depth: Some(2),
            kind: Some("function".into()),
            return_type_tags: vec!["result".into(), "owned".into()],
            effects: vec!["async".into(), "network".into()],
        };
        let json = facets_to_json(&facets);
        let parsed = facets_from_json(&json);
        assert_eq!(parsed.scope_path, facets.scope_path);
        assert_eq!(parsed.return_type_tags, facets.return_type_tags);
        assert_eq!(parsed.effects, facets.effects);
    }

    #[test]
    fn filter_matches_effects_subset() {
        let facets = SymbolFacets {
            effects: vec!["async".into(), "network".into(), "deprecated".into()],
            ..Default::default()
        };
        let filter = MemoryFacetFilter {
            require_effects: vec!["deprecated".into()],
            ..Default::default()
        };
        assert!(filter.matches(&facets));
    }

    #[test]
    fn filter_rejects_missing_effect() {
        let facets = SymbolFacets {
            effects: vec!["async".into()],
            ..Default::default()
        };
        let filter = MemoryFacetFilter {
            require_effects: vec!["unsafe".into()],
            ..Default::default()
        };
        assert!(!filter.matches(&facets));
    }

    #[test]
    fn filter_matches_scope_prefix() {
        let facets = SymbolFacets {
            scope_path: Some("crate::auth::login::validate".into()),
            ..Default::default()
        };
        let filter = MemoryFacetFilter {
            scope_path_prefix: Some("crate::auth".into()),
            ..Default::default()
        };
        assert!(filter.matches(&facets));
    }

    #[test]
    fn empty_filter_matches_anything() {
        let facets = SymbolFacets::default();
        let filter = MemoryFacetFilter::default();
        assert!(filter.matches(&facets));
    }
}
