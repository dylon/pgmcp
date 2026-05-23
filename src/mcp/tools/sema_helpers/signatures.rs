//! Structured signature descriptors for the upgraded MCP tools.
//!
//! Wraps the `symbol_parameters` + `file_symbols.return_type_*` + the
//! existing `signature` column into a single descriptor that downstream
//! tools (`public_api_surface`, `semver_break_audit`, `naming_consistency`,
//! pattern matchers, etc.) consume.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::parsing::type_tags::TypeShape;

/// One parameter as surfaced through the shadow-ASR persistence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDescriptor {
    pub position: i32,
    pub name: Option<String>,
    pub type_raw: Option<String>,
    #[serde(default)]
    pub type_tags: Vec<String>,
    #[serde(default)]
    pub type_shape: Option<TypeShape>,
    pub modifier: Option<String>,
    pub is_variadic: bool,
    pub is_self: bool,
    pub default_value: Option<String>,
}

/// A function-shaped symbol's full structural description, suitable for
/// JSON responses (`public_api_surface`), diff (`semver_break_audit`), or
/// cross-language matching (`find_duplicates`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureDescriptor {
    pub symbol_id: i64,
    pub file_id: i64,
    pub name: String,
    pub kind: String,
    pub visibility: Option<String>,
    pub scope_path: Option<String>,
    pub scope_depth: Option<i32>,
    pub signature_raw: Option<String>,
    #[serde(default)]
    pub parameters: Vec<ParamDescriptor>,
    pub return_type_raw: Option<String>,
    #[serde(default)]
    pub return_type_tags: Vec<String>,
    #[serde(default)]
    pub return_type_shape: Option<TypeShape>,
    #[serde(default)]
    pub generic_params: serde_json::Value,
    #[serde(default)]
    pub effects: Vec<String>,
}

/// Per-row tuple returned by the file_symbols query. Aliased so sqlx's
/// inferred type stays inside clippy's complexity thresholds.
type FileSymbolRow = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Vec<String>,
    Option<serde_json::Value>,
    Option<serde_json::Value>,
);

/// Per-row tuple returned by the symbol_parameters query.
type ParamRow = (
    i32,
    Option<String>,
    Option<String>,
    Vec<String>,
    Option<serde_json::Value>,
    Option<String>,
    Option<String>,
    bool,
    bool,
);

/// Fetch a `SignatureDescriptor` for a single symbol. Returns `None`
/// when the symbol does not exist.
pub async fn fetch_signature_descriptor(
    pool: &PgPool,
    symbol_id: i64,
) -> Result<Option<SignatureDescriptor>, sqlx::Error> {
    let row: Option<FileSymbolRow> = sqlx::query_as(
        "SELECT id, file_id, name, kind, visibility, scope_path, scope_depth,
                signature, return_type_raw,
                COALESCE(return_type_tags, '{}'::text[]) AS return_type_tags,
                return_type_shape, generic_params
         FROM file_symbols
         WHERE id = $1",
    )
    .bind(symbol_id)
    .fetch_optional(pool)
    .await?;
    let Some((
        id,
        file_id,
        name,
        kind,
        visibility,
        scope_path,
        scope_depth,
        signature,
        return_type_raw,
        return_type_tags,
        return_type_shape_json,
        generic_params_json,
    )) = row
    else {
        return Ok(None);
    };
    let parameters = fetch_parameters(pool, symbol_id).await?;
    let effects = fetch_effects(pool, symbol_id).await?;
    let return_type_shape: Option<TypeShape> =
        return_type_shape_json.and_then(|v| serde_json::from_value(v).ok());
    Ok(Some(SignatureDescriptor {
        symbol_id: id,
        file_id,
        name,
        kind,
        visibility,
        scope_path,
        scope_depth,
        signature_raw: signature,
        parameters,
        return_type_raw,
        return_type_tags,
        return_type_shape,
        generic_params: generic_params_json.unwrap_or(serde_json::Value::Null),
        effects,
    }))
}

async fn fetch_parameters(
    pool: &PgPool,
    symbol_id: i64,
) -> Result<Vec<ParamDescriptor>, sqlx::Error> {
    let rows: Vec<ParamRow> = sqlx::query_as(
        "SELECT position, name, type_raw,
                COALESCE(type_tags, '{}'::text[]) AS type_tags,
                type_shape, default_value, modifier, is_variadic, is_self
         FROM symbol_parameters
         WHERE symbol_id = $1
         ORDER BY position",
    )
    .bind(symbol_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                position,
                name,
                type_raw,
                type_tags,
                type_shape_json,
                default_value,
                modifier,
                is_variadic,
                is_self,
            )| ParamDescriptor {
                position,
                name,
                type_raw,
                type_tags,
                type_shape: type_shape_json.and_then(|v| serde_json::from_value(v).ok()),
                modifier,
                is_variadic,
                is_self,
                default_value,
            },
        )
        .collect())
}

async fn fetch_effects(pool: &PgPool, symbol_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT effect FROM symbol_effects WHERE symbol_id = $1 ORDER BY effect")
        .bind(symbol_id)
        .fetch_all(pool)
        .await
}

/// Compute a 64-bit structural hash of a `SignatureDescriptor`. Two
/// descriptors with the same shape (parameter count, parameter type
/// shapes in order, return type shape, effect set) hash to the same
/// `u64`. Names and `signature_raw` are deliberately excluded so the
/// hash is comparable across languages.
pub fn signature_shape_hash(sig: &SignatureDescriptor) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    sig.parameters.len().hash(&mut h);
    for p in &sig.parameters {
        // Hash structural shape (constructor + arity tree). When shape is
        // missing, hash the type tags instead so we still cluster.
        if let Some(shape) = &p.type_shape {
            shape.structural_hash().hash(&mut h);
        } else {
            let mut tags = p.type_tags.clone();
            tags.sort();
            for t in tags {
                t.hash(&mut h);
            }
        }
        p.is_variadic.hash(&mut h);
        p.is_self.hash(&mut h);
    }
    if let Some(rt) = &sig.return_type_shape {
        rt.structural_hash().hash(&mut h);
    } else {
        let mut tags = sig.return_type_tags.clone();
        tags.sort();
        for t in tags {
            t.hash(&mut h);
        }
    }
    let mut effs = sig.effects.clone();
    effs.sort();
    for e in effs {
        e.hash(&mut h);
    }
    h.finish()
}

/// What changed between two signatures. Used by `semver_break_audit` /
/// `release_api_stability` to classify breaking changes precisely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignatureDelta {
    pub parameter_added: Vec<String>,
    pub parameter_removed: Vec<String>,
    pub parameter_type_changed: Vec<String>,
    pub return_type_changed: bool,
    pub effect_added: Vec<String>,
    pub effect_removed: Vec<String>,
    /// Whether this delta represents a breaking change to the API surface.
    pub is_breaking: bool,
}

/// Compute the delta between two signatures. The classification is
/// conservative: anything that changes parameter count, parameter type
/// shape, return type shape, or effect set is considered a candidate
/// for breaking. The `is_breaking` flag rolls up the categories.
pub fn signature_diff(before: &SignatureDescriptor, after: &SignatureDescriptor) -> SignatureDelta {
    let mut delta = SignatureDelta::default();

    let before_names: Vec<String> = before
        .parameters
        .iter()
        .map(|p| p.name.clone().unwrap_or_else(|| format!("_{}", p.position)))
        .collect();
    let after_names: Vec<String> = after
        .parameters
        .iter()
        .map(|p| p.name.clone().unwrap_or_else(|| format!("_{}", p.position)))
        .collect();

    for (i, p_after) in after.parameters.iter().enumerate() {
        let name = &after_names[i];
        if let Some(p_before) = before.parameters.get(i) {
            // Same position — compare shapes.
            let shape_before = p_before
                .type_shape
                .as_ref()
                .map(|s| s.structural_hash())
                .unwrap_or(0);
            let shape_after = p_after
                .type_shape
                .as_ref()
                .map(|s| s.structural_hash())
                .unwrap_or(0);
            if shape_before != shape_after {
                delta.parameter_type_changed.push(name.clone());
            }
        } else {
            // Added position.
            delta.parameter_added.push(name.clone());
        }
    }
    for (i, name) in before_names.iter().enumerate() {
        if i >= after.parameters.len() {
            delta.parameter_removed.push(name.clone());
        }
    }

    let rt_before = before
        .return_type_shape
        .as_ref()
        .map(|s| s.structural_hash())
        .unwrap_or(0);
    let rt_after = after
        .return_type_shape
        .as_ref()
        .map(|s| s.structural_hash())
        .unwrap_or(0);
    delta.return_type_changed = rt_before != rt_after;

    let before_eff: std::collections::HashSet<&String> = before.effects.iter().collect();
    let after_eff: std::collections::HashSet<&String> = after.effects.iter().collect();
    for e in &after_eff - &before_eff {
        delta.effect_added.push(e.clone());
    }
    for e in &before_eff - &after_eff {
        delta.effect_removed.push(e.clone());
    }

    delta.is_breaking = !delta.parameter_added.is_empty()
        || !delta.parameter_removed.is_empty()
        || !delta.parameter_type_changed.is_empty()
        || delta.return_type_changed
        || !delta.effect_added.is_empty();

    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_sig(name: &str) -> SignatureDescriptor {
        SignatureDescriptor {
            symbol_id: 1,
            file_id: 1,
            name: name.into(),
            kind: "function".into(),
            visibility: None,
            scope_path: None,
            scope_depth: Some(0),
            signature_raw: None,
            parameters: Vec::new(),
            return_type_raw: None,
            return_type_tags: Vec::new(),
            return_type_shape: None,
            generic_params: serde_json::Value::Null,
            effects: Vec::new(),
        }
    }

    fn param(position: i32, name: &str, shape_ctor: &str) -> ParamDescriptor {
        ParamDescriptor {
            position,
            name: Some(name.into()),
            type_raw: Some(shape_ctor.into()),
            type_tags: vec![],
            type_shape: Some(TypeShape::leaf(shape_ctor)),
            modifier: None,
            is_variadic: false,
            is_self: false,
            default_value: None,
        }
    }

    #[test]
    fn shape_hash_equal_for_identical_shapes() {
        let mut a = empty_sig("f");
        a.parameters = vec![param(0, "x", "i32"), param(1, "y", "str")];
        a.return_type_shape = Some(TypeShape::leaf("bool"));
        let mut b = empty_sig("g"); // different name doesn't matter
        b.parameters = vec![param(0, "a", "i32"), param(1, "b", "str")];
        b.return_type_shape = Some(TypeShape::leaf("bool"));
        assert_eq!(signature_shape_hash(&a), signature_shape_hash(&b));
    }

    #[test]
    fn shape_hash_differs_when_arity_differs() {
        let mut a = empty_sig("f");
        a.parameters = vec![param(0, "x", "i32")];
        let mut b = empty_sig("f");
        b.parameters = vec![param(0, "x", "i32"), param(1, "y", "i32")];
        assert_ne!(signature_shape_hash(&a), signature_shape_hash(&b));
    }

    #[test]
    fn shape_hash_differs_when_return_type_differs() {
        let mut a = empty_sig("f");
        a.return_type_shape = Some(TypeShape::leaf("bool"));
        let mut b = empty_sig("f");
        b.return_type_shape = Some(TypeShape::leaf("i32"));
        assert_ne!(signature_shape_hash(&a), signature_shape_hash(&b));
    }

    #[test]
    fn diff_detects_added_parameter() {
        let mut before = empty_sig("f");
        before.parameters = vec![param(0, "x", "i32")];
        let mut after = empty_sig("f");
        after.parameters = vec![param(0, "x", "i32"), param(1, "y", "str")];
        let delta = signature_diff(&before, &after);
        assert_eq!(delta.parameter_added, vec!["y".to_string()]);
        assert!(delta.is_breaking);
    }

    #[test]
    fn diff_detects_removed_parameter() {
        let mut before = empty_sig("f");
        before.parameters = vec![param(0, "x", "i32"), param(1, "y", "str")];
        let mut after = empty_sig("f");
        after.parameters = vec![param(0, "x", "i32")];
        let delta = signature_diff(&before, &after);
        assert_eq!(delta.parameter_removed, vec!["y".to_string()]);
        assert!(delta.is_breaking);
    }

    #[test]
    fn diff_detects_type_change() {
        let mut before = empty_sig("f");
        before.parameters = vec![param(0, "x", "i32")];
        let mut after = empty_sig("f");
        after.parameters = vec![param(0, "x", "i64")];
        let delta = signature_diff(&before, &after);
        assert_eq!(delta.parameter_type_changed, vec!["x".to_string()]);
        assert!(delta.is_breaking);
    }

    #[test]
    fn diff_detects_return_type_change() {
        let mut before = empty_sig("f");
        before.return_type_shape = Some(TypeShape::leaf("bool"));
        let mut after = empty_sig("f");
        after.return_type_shape = Some(TypeShape::leaf("i32"));
        let delta = signature_diff(&before, &after);
        assert!(delta.return_type_changed);
        assert!(delta.is_breaking);
    }

    #[test]
    fn diff_detects_effect_added() {
        let mut before = empty_sig("f");
        let mut after = empty_sig("f");
        after.effects = vec!["unsafe".to_string()];
        let delta = signature_diff(&before, &after);
        assert!(delta.effect_added.contains(&"unsafe".to_string()));
        assert!(delta.is_breaking);
        // Effect removal isn't breaking but should be reported.
        before = empty_sig("f");
        before.effects = vec!["unsafe".to_string()];
        let after = empty_sig("f");
        let delta = signature_diff(&before, &after);
        assert!(delta.effect_removed.contains(&"unsafe".to_string()));
    }

    #[test]
    fn diff_clean_when_identical() {
        let mut sig = empty_sig("f");
        sig.parameters = vec![param(0, "x", "i32")];
        sig.return_type_shape = Some(TypeShape::leaf("bool"));
        sig.effects = vec!["pure".to_string()];
        let delta = signature_diff(&sig, &sig);
        assert!(!delta.is_breaking);
        assert!(delta.parameter_added.is_empty());
        assert!(delta.parameter_removed.is_empty());
        assert!(delta.parameter_type_changed.is_empty());
        assert!(!delta.return_type_changed);
        assert!(delta.effect_added.is_empty());
        assert!(delta.effect_removed.is_empty());
    }
}
