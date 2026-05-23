//! Type-tag and effect vocabulary for the unified semantic representation
//! (shadow ASR — see ADR-003 / the plan at
//! `~/.claude/plans/would-translating-the-asts-cosmic-quill.md`).
//!
//! This module is the canonical source of:
//!
//! - The **type-tag vocabulary**: open-set tags that label semantic facets
//!   of a parsed type (`int`, `container`, `mutex`, `option`, `metta_typed`,
//!   `linear`, …). Stored on `symbol_parameters.type_tags` and
//!   `file_symbols.return_type_tags`.
//! - The **effect vocabulary**: open-set effects that label what a symbol
//!   does (`async`, `unsafe`, `may_panic`, `channel_send_persistent`,
//!   `term_rewrite`, …). Stored in the `symbol_effects` table.
//! - The **`TypeShape`** struct: structural decomposition of a parsed type
//!   stored as JSONB on `symbol_parameters.type_shape` /
//!   `file_symbols.return_type_shape`.
//! - **Tagger helpers** for backends — `tag_container_like`,
//!   `tag_future_like`, etc. — that assemble cross-cutting tag sets
//!   consistently. Backends still own their per-language mapping rules; the
//!   helpers just keep multi-tag combos uniform.
//!
//! All names emitted by a backend MUST be present in this module's seed
//! lists. The migration `shadow_asr_v1` populates `type_tag_catalog` and
//! `effect_catalog` from these lists; backends import the `&'static str`
//! constants so typos are caught at compile time.

// Same idiom as `src/parsing/symbols.rs`: helpers + re-exports are consumed
// by the persistence layer + backends in Phase B+. Allowing dead_code at file
// scope keeps `clippy -D warnings` green until those consumers land.
#![allow(dead_code)]

pub mod shape;
pub mod vocabulary;

#[allow(unused_imports)]
pub use shape::{ShapeDescendants, TypeShape};
#[allow(unused_imports)]
pub use vocabulary::{
    SEED_EFFECTS, SEED_TYPE_TAGS, TagDef, TagOrigin, effect, is_known_tag_or_effect, type_tag,
};

/// Tag set built up by backends to attach to a `Parameter` or `ReturnType`.
///
/// `&'static str` because every tag comes from the `vocabulary::TAG_*`
/// constants — there is no runtime tag minting. The persistence layer
/// converts to `Vec<&str>` for `text[]` columns; backends pass slices.
pub type TagSet = Vec<&'static str>;

/// Helper: tag a container-like type with the canonical compound set
/// `[container, owned, dynamic, indexed]` (a `Vec<T>` / `list[T]` /
/// `ArrayList<T>` / `[]T` shape). Pass `additional` to compose extra facets
/// (e.g. for a `BTreeMap`, pass `[KEYED, ORDERED]`).
pub fn tag_container_like(additional: &[&'static str]) -> TagSet {
    let mut tags: TagSet = vec![
        vocabulary::TAG_CONTAINER,
        vocabulary::TAG_OWNED,
        vocabulary::TAG_DYNAMIC,
        vocabulary::TAG_INDEXED,
    ];
    tags.extend_from_slice(additional);
    tags.sort();
    tags.dedup();
    tags
}

/// Helper: tag a map / dictionary type with `[container, keyed, owned, dynamic]`.
/// Combine with `[ordered]` for `BTreeMap`/`LinkedHashMap`, or `[unordered]`
/// for `HashMap`/`dict`.
pub fn tag_map_like(additional: &[&'static str]) -> TagSet {
    let mut tags: TagSet = vec![
        vocabulary::TAG_CONTAINER,
        vocabulary::TAG_KEYED,
        vocabulary::TAG_OWNED,
        vocabulary::TAG_DYNAMIC,
    ];
    tags.extend_from_slice(additional);
    tags.sort();
    tags.dedup();
    tags
}

/// Helper: tag a set type with `[container, unordered, owned, dynamic]`. Pass
/// `[ordered]` for sorted-set variants.
pub fn tag_set_like(additional: &[&'static str]) -> TagSet {
    let mut tags: TagSet = vec![
        vocabulary::TAG_CONTAINER,
        vocabulary::TAG_UNORDERED,
        vocabulary::TAG_OWNED,
        vocabulary::TAG_DYNAMIC,
    ];
    tags.extend_from_slice(additional);
    tags.sort();
    tags.dedup();
    tags
}

/// Helper: tag a future / awaitable as `[future, async]`.
pub fn tag_future_like() -> TagSet {
    let mut tags: TagSet = vec![vocabulary::TAG_FUTURE, vocabulary::TAG_ASYNC];
    tags.sort();
    tags
}

/// Helper: tag a smart pointer as `[smart_pointer, owned, shared]`. Pass
/// `[mutable_ref]` for `Arc<Mutex<_>>`-style interior mutability.
pub fn tag_smart_pointer(additional: &[&'static str]) -> TagSet {
    let mut tags: TagSet = vec![
        vocabulary::TAG_SMART_POINTER,
        vocabulary::TAG_OWNED,
        vocabulary::TAG_SHARED,
    ];
    tags.extend_from_slice(additional);
    tags.sort();
    tags.dedup();
    tags
}

/// Helper: tag an Option-like type as `[option, null_like]`.
pub fn tag_option_like() -> TagSet {
    let mut tags: TagSet = vec![vocabulary::TAG_OPTION, vocabulary::TAG_NULL_LIKE];
    tags.sort();
    tags
}

/// Helper: tag a Result/Either-like type as `[result, sum_type]`.
pub fn tag_result_like() -> TagSet {
    let mut tags: TagSet = vec![vocabulary::TAG_RESULT, vocabulary::TAG_SUM_TYPE];
    tags.sort();
    tags
}

/// Validate a tag set against the seed vocabulary. Returns the first unknown
/// tag, or `None` when all tags are known. Used by the persistence layer to
/// reject bogus backend emissions before they reach Postgres.
pub fn first_unknown_type_tag<'a>(tags: &'a [&'a str]) -> Option<&'a str> {
    tags.iter()
        .copied()
        .find(|t| vocabulary::type_tag(t).is_none())
}

/// Validate an effect set against the seed vocabulary. Returns the first
/// unknown effect, or `None` when all are known.
pub fn first_unknown_effect<'a>(effects: &'a [&'a str]) -> Option<&'a str> {
    effects
        .iter()
        .copied()
        .find(|t| vocabulary::effect(t).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_helper_includes_canonical_facets() {
        let tags = tag_container_like(&[vocabulary::TAG_BYTES]);
        assert!(tags.contains(&vocabulary::TAG_CONTAINER));
        assert!(tags.contains(&vocabulary::TAG_OWNED));
        assert!(tags.contains(&vocabulary::TAG_DYNAMIC));
        assert!(tags.contains(&vocabulary::TAG_INDEXED));
        assert!(tags.contains(&vocabulary::TAG_BYTES));
    }

    #[test]
    fn container_helper_dedupes_repeated_tags() {
        let tags = tag_container_like(&[vocabulary::TAG_CONTAINER, vocabulary::TAG_OWNED]);
        let mut sorted = tags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(tags.len(), sorted.len(), "helper failed to dedupe");
    }

    #[test]
    fn map_helper_marks_keyed() {
        let tags = tag_map_like(&[vocabulary::TAG_UNORDERED]);
        assert!(tags.contains(&vocabulary::TAG_KEYED));
        assert!(tags.contains(&vocabulary::TAG_UNORDERED));
        assert!(!tags.contains(&vocabulary::TAG_INDEXED));
    }

    #[test]
    fn set_helper_marks_unordered_by_default() {
        let tags = tag_set_like(&[]);
        assert!(tags.contains(&vocabulary::TAG_UNORDERED));
        assert!(tags.contains(&vocabulary::TAG_CONTAINER));
    }

    #[test]
    fn future_helper_marks_async() {
        let tags = tag_future_like();
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&vocabulary::TAG_FUTURE));
        assert!(tags.contains(&vocabulary::TAG_ASYNC));
    }

    #[test]
    fn smart_pointer_helper_supports_mutable_interior() {
        let tags = tag_smart_pointer(&[vocabulary::TAG_MUTABLE_REF, vocabulary::TAG_MUTEX]);
        assert!(tags.contains(&vocabulary::TAG_SMART_POINTER));
        assert!(tags.contains(&vocabulary::TAG_MUTEX));
        assert!(tags.contains(&vocabulary::TAG_MUTABLE_REF));
    }

    #[test]
    fn option_helper_marks_null_like() {
        let tags = tag_option_like();
        assert!(tags.contains(&vocabulary::TAG_OPTION));
        assert!(tags.contains(&vocabulary::TAG_NULL_LIKE));
    }

    #[test]
    fn result_helper_marks_sum_type() {
        let tags = tag_result_like();
        assert!(tags.contains(&vocabulary::TAG_RESULT));
        assert!(tags.contains(&vocabulary::TAG_SUM_TYPE));
    }

    #[test]
    fn unknown_tag_detection() {
        let good: &[&str] = &[vocabulary::TAG_INT, vocabulary::TAG_OWNED];
        assert!(first_unknown_type_tag(good).is_none());

        let bad: &[&str] = &[vocabulary::TAG_INT, "totally_made_up_tag"];
        assert_eq!(first_unknown_type_tag(bad), Some("totally_made_up_tag"));
    }

    #[test]
    fn unknown_effect_detection() {
        let good: &[&str] = &[vocabulary::EFFECT_ASYNC, vocabulary::EFFECT_MAY_PANIC];
        assert!(first_unknown_effect(good).is_none());

        let bad: &[&str] = &[vocabulary::EFFECT_ASYNC, "bogus_effect"];
        assert_eq!(first_unknown_effect(bad), Some("bogus_effect"));
    }

    #[test]
    fn re_exports_resolve() {
        // Compile-time check that the public surface includes the types
        // downstream callers expect.
        let _: &[TagDef] = SEED_TYPE_TAGS;
        let _: &[TagDef] = SEED_EFFECTS;
        let _: TypeShape = TypeShape::leaf("x");
        let _ = TagOrigin::Universal;
        let _ = is_known_tag_or_effect("int");
        let _ = type_tag("int");
        let _ = effect("async");
    }
}
