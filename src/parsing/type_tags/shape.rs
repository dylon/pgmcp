//! Structural decomposition of a type expression.
//!
//! A `TypeShape` is the language-agnostic skeleton of a parsed type:
//! `Vec<Result<u8, IoError>>` becomes
//!
//! ```text
//! TypeShape {
//!   constructor: "Vec",
//!   args: [
//!     TypeShape {
//!       constructor: "Result",
//!       args: [
//!         TypeShape { constructor: "u8", args: [], raw: Some("u8") },
//!         TypeShape { constructor: "IoError", args: [], raw: Some("IoError") },
//!       ],
//!       raw: Some("Result<u8, IoError>"),
//!     }
//!   ],
//!   raw: Some("Vec<Result<u8, IoError>>"),
//! }
//! ```
//!
//! Shape values are stored in PostgreSQL as JSONB on
//! `symbol_parameters.type_shape` and `file_symbols.return_type_shape`. The
//! `signature_shape_hash` produces a structural hash for equality / clustering
//! that ignores `raw` (so Rust `Vec<u8>` and Python `list[int]` hash to
//! different values, but Rust `Vec<u8>` and Rust `std::vec::Vec<u8>` hash to
//! the same value — modulo per-backend constructor normalization).

// Same idiom as `src/parsing/symbols.rs`: TypeShape and its constructors are
// consumed by the persistence layer + backends in Phase B+. Allowing
// dead_code at file scope keeps `clippy -D warnings` green until those
// consumers land.
#![allow(dead_code)]

use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

/// Structural decomposition of a parsed type expression.
///
/// `serde(skip_serializing_if = ...)` is intentionally NOT used on
/// these fields. Postcard's wire format is positional (no field names),
/// so skipping fields at serialization time would desynchronize the
/// deserializer when the same `TypeShape` rides inside a `Symbol` that
/// gets round-tripped through postcard (golden-fixture tests). Always
/// emit all three fields; empty `Vec` / `None` `Option` encode in 1-2
/// bytes apiece, so the wire-format cost is negligible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeShape {
    /// Bare constructor identifier, normalized by the backend. For example
    /// Rust's `std::vec::Vec` → `"Vec"`; Python's `typing.List` → `"List"`.
    /// Backends are responsible for choosing one canonical name per
    /// language; cross-language equivalence is enforced by tests, not by
    /// shared rules.
    pub constructor: String,
    /// Type arguments, in source order. Empty for nullary constructors.
    #[serde(default)]
    pub args: Vec<TypeShape>,
    /// Round-trippable source slice of the original type expression. Optional
    /// because backends that derive a shape from non-textual sources (Rust
    /// `syn`'s `Type` enum) may not carry the exact source text.
    #[serde(default)]
    pub raw: Option<String>,
}

impl TypeShape {
    /// Leaf shape: a bare constructor with no args.
    pub fn leaf(constructor: impl Into<String>) -> Self {
        Self {
            constructor: constructor.into(),
            args: Vec::new(),
            raw: None,
        }
    }

    /// Leaf shape with an attached round-trippable raw form.
    pub fn leaf_raw(constructor: impl Into<String>, raw: impl Into<String>) -> Self {
        Self {
            constructor: constructor.into(),
            args: Vec::new(),
            raw: Some(raw.into()),
        }
    }

    /// Constructor applied to type arguments.
    pub fn applied(constructor: impl Into<String>, args: Vec<TypeShape>) -> Self {
        Self {
            constructor: constructor.into(),
            args,
            raw: None,
        }
    }

    /// Constructor applied to type arguments with an attached raw form.
    pub fn applied_raw(
        constructor: impl Into<String>,
        args: Vec<TypeShape>,
        raw: impl Into<String>,
    ) -> Self {
        Self {
            constructor: constructor.into(),
            args,
            raw: Some(raw.into()),
        }
    }

    /// Iterator over `self` and every descendant in pre-order.
    pub fn iter_descendants(&self) -> ShapeDescendants<'_> {
        ShapeDescendants { stack: vec![self] }
    }

    /// Arity = number of direct type arguments.
    pub fn arity(&self) -> usize {
        self.args.len()
    }

    /// Total number of constructor nodes (self + all descendants).
    pub fn size(&self) -> usize {
        1 + self.args.iter().map(TypeShape::size).sum::<usize>()
    }

    /// Maximum nesting depth (1 for a leaf).
    pub fn depth(&self) -> usize {
        1 + self.args.iter().map(TypeShape::depth).max().unwrap_or(0)
    }

    /// Structural hash ignoring `raw`. Two shapes with the same constructor
    /// tree hash to the same `u64`. Used for cross-language clone detection.
    pub fn structural_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        self.hash_into(&mut h);
        h.finish()
    }

    fn hash_into<H: Hasher>(&self, h: &mut H) {
        self.constructor.hash(h);
        self.args.len().hash(h);
        for a in &self.args {
            a.hash_into(h);
        }
    }
}

/// Pre-order traversal iterator over `TypeShape` nodes.
pub struct ShapeDescendants<'a> {
    stack: Vec<&'a TypeShape>,
}

impl<'a> Iterator for ShapeDescendants<'a> {
    type Item = &'a TypeShape;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.stack.pop()?;
        // Push args in reverse so the first arg is visited next.
        for child in next.args.iter().rev() {
            self.stack.push(child);
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_result_u8_ioerror() -> TypeShape {
        TypeShape::applied(
            "Vec",
            vec![TypeShape::applied(
                "Result",
                vec![TypeShape::leaf("u8"), TypeShape::leaf("IoError")],
            )],
        )
    }

    #[test]
    fn leaf_has_arity_zero_size_one_depth_one() {
        let s = TypeShape::leaf("i32");
        assert_eq!(s.arity(), 0);
        assert_eq!(s.size(), 1);
        assert_eq!(s.depth(), 1);
        assert!(s.args.is_empty());
        assert_eq!(s.constructor, "i32");
    }

    #[test]
    fn nested_shape_has_correct_size_and_depth() {
        let s = vec_result_u8_ioerror();
        // Vec, Result, u8, IoError = 4
        assert_eq!(s.size(), 4);
        // Vec -> Result -> u8 = 3 levels
        assert_eq!(s.depth(), 3);
        assert_eq!(s.arity(), 1);
    }

    #[test]
    fn structural_hash_ignores_raw() {
        let a = TypeShape::leaf("i32");
        let b = TypeShape::leaf_raw("i32", "i32");
        assert_eq!(a.structural_hash(), b.structural_hash());

        let c = TypeShape::applied_raw("Vec", vec![TypeShape::leaf("u8")], "std::vec::Vec<u8>");
        let d = TypeShape::applied("Vec", vec![TypeShape::leaf("u8")]);
        assert_eq!(c.structural_hash(), d.structural_hash());
    }

    #[test]
    fn structural_hash_distinguishes_different_shapes() {
        let a = TypeShape::leaf("i32");
        let b = TypeShape::leaf("i64");
        assert_ne!(a.structural_hash(), b.structural_hash());

        let c = TypeShape::applied("Vec", vec![TypeShape::leaf("u8")]);
        let d = TypeShape::applied("Vec", vec![TypeShape::leaf("u16")]);
        assert_ne!(c.structural_hash(), d.structural_hash());
    }

    #[test]
    fn structural_hash_is_order_sensitive_on_args() {
        // Result<T, E> ≠ Result<E, T> — argument order is part of identity.
        let a = TypeShape::applied(
            "Result",
            vec![TypeShape::leaf("u8"), TypeShape::leaf("IoError")],
        );
        let b = TypeShape::applied(
            "Result",
            vec![TypeShape::leaf("IoError"), TypeShape::leaf("u8")],
        );
        assert_ne!(a.structural_hash(), b.structural_hash());
    }

    #[test]
    fn iter_descendants_visits_every_node_in_preorder() {
        let s = vec_result_u8_ioerror();
        let visited: Vec<&str> = s
            .iter_descendants()
            .map(|n| n.constructor.as_str())
            .collect();
        assert_eq!(visited, vec!["Vec", "Result", "u8", "IoError"]);
    }

    #[test]
    fn json_round_trip_preserves_structure_and_raw() {
        let s = TypeShape::applied_raw(
            "HashMap",
            vec![TypeShape::leaf("String"), TypeShape::leaf("i64")],
            "HashMap<String, i64>",
        );
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: TypeShape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, s);
    }

    #[test]
    fn json_emits_all_fields() {
        // After the postcard-compatibility fix that removed
        // `skip_serializing_if`, JSON serialization always emits every
        // field. The wire-format cost is a few extra bytes per shape;
        // the win is that the same struct round-trips through both
        // JSONB (Postgres) and postcard (golden fixtures) without
        // discrepancy. The previous "omit-on-default" behavior caused
        // a Symbol vector to fail to deserialize from postcard when
        // any contained TypeShape had defaulted args / raw.
        let s = TypeShape::leaf("i32");
        let json = serde_json::to_string(&s).expect("serialize");
        assert_eq!(json, "{\"constructor\":\"i32\",\"args\":[],\"raw\":null}");
    }

    #[test]
    fn cross_language_equivalence_via_constructor_normalization() {
        // Rust Vec<u8> and Python list[int] are NOT equivalent (different
        // constructor names). Backends are free to map both to `array<int>`
        // by emitting that constructor; the shape mechanism cooperates.
        let rust = TypeShape::applied("Vec", vec![TypeShape::leaf("u8")]);
        let python = TypeShape::applied("list", vec![TypeShape::leaf("int")]);
        assert_ne!(rust.structural_hash(), python.structural_hash());

        let normalized_rust = TypeShape::applied("array", vec![TypeShape::leaf("int")]);
        let normalized_python = TypeShape::applied("array", vec![TypeShape::leaf("int")]);
        assert_eq!(
            normalized_rust.structural_hash(),
            normalized_python.structural_hash()
        );
    }
}
