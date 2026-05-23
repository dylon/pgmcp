//! Map `syn` AST nodes to the shadow-ASR semantic structures
//! (`Parameter`, `ReturnType`, `GenericParam`, type tags, effects).
//!
//! Lives in `src/parsing/rust/` because the mapping rules are
//! Rust-specific. The output types come from the language-agnostic
//! `crate::parsing::symbols` module so they round-trip cleanly into the
//! database (`symbol_parameters`, `file_symbols.return_type_*`,
//! `symbol_effects`).
//!
//! See `~/.claude/plans/would-translating-the-asts-cosmic-quill.md`
//! Phase B for the broader rollout context.

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::{Attribute, FnArg, GenericParam as SynGenericParam, ReturnType, Signature, Type};

use crate::parsing::symbols::{
    GenericParam, ParamModifier, Parameter, ReturnType as SemReturnType,
};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

use super::helpers::type_to_string;

/// Convert a `syn::Type` into a structural `TypeShape`. Recursive — nested
/// generics, references, tuples, arrays, and pointers all preserve their
/// constructor + arg structure.
pub(super) fn type_to_shape(ty: &Type) -> TypeShape {
    let raw = type_to_string(ty);
    match ty {
        Type::Path(p) => {
            let Some(last_seg) = p.path.segments.last() else {
                return TypeShape::leaf_raw("_unknown", raw);
            };
            let constructor = last_seg.ident.to_string();
            let args = match &last_seg.arguments {
                syn::PathArguments::AngleBracketed(ab) => ab
                    .args
                    .iter()
                    .filter_map(|a| match a {
                        syn::GenericArgument::Type(t) => Some(type_to_shape(t)),
                        _ => None,
                    })
                    .collect(),
                syn::PathArguments::Parenthesized(par) => {
                    // `Fn(T1, T2) -> R` style — model as args = [T1, T2, R].
                    let mut args: Vec<TypeShape> = par.inputs.iter().map(type_to_shape).collect();
                    if let ReturnType::Type(_, ret) = &par.output {
                        args.push(type_to_shape(ret));
                    }
                    args
                }
                syn::PathArguments::None => Vec::new(),
            };
            TypeShape::applied_raw(constructor, args, raw)
        }
        Type::Reference(r) => {
            let inner = type_to_shape(&r.elem);
            let constructor = if r.mutability.is_some() { "&mut" } else { "&" };
            TypeShape::applied_raw(constructor, vec![inner], raw)
        }
        Type::Tuple(t) => {
            if t.elems.is_empty() {
                TypeShape::leaf_raw("Unit", raw)
            } else {
                let args: Vec<TypeShape> = t.elems.iter().map(type_to_shape).collect();
                TypeShape::applied_raw("Tuple", args, raw)
            }
        }
        Type::Slice(s) => TypeShape::applied_raw("Slice", vec![type_to_shape(&s.elem)], raw),
        Type::Array(a) => TypeShape::applied_raw("Array", vec![type_to_shape(&a.elem)], raw),
        Type::Ptr(p) => {
            let constructor = if p.mutability.is_some() {
                "*mut"
            } else {
                "*const"
            };
            TypeShape::applied_raw(constructor, vec![type_to_shape(&p.elem)], raw)
        }
        Type::BareFn(_) => TypeShape::leaf_raw("BareFn", raw),
        Type::TraitObject(_) => TypeShape::leaf_raw("dyn", raw),
        Type::ImplTrait(_) => TypeShape::leaf_raw("impl", raw),
        Type::Paren(p) => type_to_shape(&p.elem),
        Type::Group(g) => type_to_shape(&g.elem),
        Type::Infer(_) => TypeShape::leaf_raw("_", raw),
        Type::Macro(_) => TypeShape::leaf_raw("Macro", raw),
        Type::Never(_) => TypeShape::leaf_raw("Never", raw),
        Type::Verbatim(_) => TypeShape::leaf_raw("Verbatim", raw),
        _ => TypeShape::leaf_raw("_unknown", raw),
    }
}

/// Map a `syn::Type` to the canonical type-tag set. Tags compose — a
/// `&mut Vec<u8>` gets `[reference, mutable_ref, container, owned, dynamic,
/// indexed, bytes]`. Caller receives an owned `Vec<String>` (the
/// persistence layer takes `text[]`).
pub(super) fn type_tags_for(ty: &Type) -> Vec<String> {
    let mut tags: Vec<&'static str> = Vec::new();
    populate_tags(ty, &mut tags);
    let mut owned: Vec<String> = tags.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

fn populate_tags(ty: &Type, tags: &mut Vec<&'static str>) {
    match ty {
        Type::Path(p) => {
            let Some(last_seg) = p.path.segments.last() else {
                tags.push(v::TAG_UNKNOWN);
                return;
            };
            let constructor = last_seg.ident.to_string();
            tag_constructor(&constructor, tags);
            // Recurse into type arguments to pick up element-type tags
            // (e.g. `Vec<u8>` gets both `container,...` and `bytes`).
            if let syn::PathArguments::AngleBracketed(ab) = &last_seg.arguments {
                for a in &ab.args {
                    if let syn::GenericArgument::Type(t) = a {
                        // Only propagate primitive element tags so we don't
                        // explode for nested `Vec<HashMap<K,V>>`. Limit to
                        // primitives at the leaf to keep the tag set focused.
                        let mut leaf_tags = Vec::new();
                        if let Type::Path(pp) = t
                            && let Some(seg) = pp.path.segments.last()
                        {
                            let prim = seg.ident.to_string();
                            tag_primitive(&prim, &mut leaf_tags);
                        }
                        tags.extend(leaf_tags);
                    }
                }
            }
        }
        Type::Reference(r) => {
            tags.push(v::TAG_REFERENCE);
            if r.mutability.is_some() {
                tags.push(v::TAG_MUTABLE_REF);
            } else {
                tags.push(v::TAG_BORROWED);
            }
            populate_tags(&r.elem, tags);
        }
        Type::Tuple(t) => {
            if t.elems.is_empty() {
                tags.push(v::TAG_UNIT);
            } else {
                tags.push(v::TAG_PRODUCT_TYPE);
            }
        }
        Type::Slice(_) => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_BORROWED);
        }
        Type::Array(_) => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_FIXED_SIZE);
            tags.push(v::TAG_OWNED);
        }
        Type::Ptr(_) => {
            tags.push(v::TAG_POINTER);
            tags.push(v::TAG_UNKNOWN);
        }
        Type::BareFn(_) => tags.push(v::TAG_FUNCTION),
        Type::TraitObject(_) | Type::ImplTrait(_) => {
            tags.push(v::TAG_EXISTENTIAL);
            tags.push(v::TAG_INTERFACE);
        }
        Type::Paren(p) => populate_tags(&p.elem, tags),
        Type::Group(g) => populate_tags(&g.elem, tags),
        Type::Never(_) => tags.push(v::TAG_NEVER),
        _ => tags.push(v::TAG_UNKNOWN),
    }
}

/// Tag a Rust path constructor name (`Vec`, `HashMap`, `Arc`, `i32`, …)
/// with its compound tag set.
fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    // Primitive scalars
    if tag_primitive(name, tags) {
        return;
    }
    match name {
        // ── Containers ─────────────────────────────────────────────
        "Vec" | "VecDeque" => {
            push_container_tags(tags);
            tags.push(v::TAG_ORDERED);
            tags.push(v::TAG_INDEXED);
        }
        "Box" => {
            push_smart_pointer_tags(tags);
            tags.push(v::TAG_UNIQUE);
        }
        "Rc" => {
            push_smart_pointer_tags(tags);
            tags.push(v::TAG_SHARED);
        }
        "Arc" => {
            push_smart_pointer_tags(tags);
            tags.push(v::TAG_SHARED);
            tags.push(v::TAG_CONCURRENCY);
        }
        "Weak" => tags.push(v::TAG_WEAK),
        "Mutex" | "RwLock" => {
            tags.push(v::TAG_MUTEX);
            tags.push(v::TAG_CONCURRENCY);
            tags.push(v::TAG_MUTABLE_REF);
        }
        "AtomicBool" | "AtomicI8" | "AtomicI16" | "AtomicI32" | "AtomicI64" | "AtomicIsize"
        | "AtomicU8" | "AtomicU16" | "AtomicU32" | "AtomicU64" | "AtomicUsize" | "AtomicPtr" => {
            tags.push(v::TAG_ATOMIC);
            tags.push(v::TAG_CONCURRENCY);
        }
        "HashMap" | "BTreeMap" | "IndexMap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "BTreeMap" {
                tags.push(v::TAG_ORDERED);
            } else {
                tags.push(v::TAG_UNORDERED);
            }
        }
        "HashSet" | "BTreeSet" | "IndexSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "BTreeSet" {
                tags.push(v::TAG_ORDERED);
            } else {
                tags.push(v::TAG_UNORDERED);
            }
        }
        // ── Algebraic ──────────────────────────────────────────────
        "Option" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
        }
        "Result" => {
            tags.push(v::TAG_RESULT);
            tags.push(v::TAG_SUM_TYPE);
        }
        // ── Computation ────────────────────────────────────────────
        "Future" | "Pin" | "BoxFuture" | "LocalBoxFuture" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
        }
        "Stream" | "BoxStream" | "LocalBoxStream" => {
            tags.push(v::TAG_STREAM);
            tags.push(v::TAG_ASYNC);
            tags.push(v::TAG_ITERATOR);
        }
        "Iterator" | "IntoIterator" => tags.push(v::TAG_ITERATOR),
        // ── Channel-shape ──────────────────────────────────────────
        "Sender" | "Receiver" | "UnboundedSender" | "UnboundedReceiver" | "SyncSender" => {
            tags.push(v::TAG_CHANNEL);
            tags.push(v::TAG_CONCURRENCY);
        }
        // ── String shape ───────────────────────────────────────────
        "String" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        "PathBuf" | "OsString" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_FILESYSTEM);
        }
        "Path" | "OsStr" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_BORROWED);
            tags.push(v::TAG_FILESYSTEM);
        }
        "PhantomData" => tags.push(v::TAG_PHANTOM),
        _ => {
            // Unknown constructor — fall through; the caller can still see
            // `type_raw` for downstream review.
        }
    }
}

/// Returns `true` if `name` is a recognized primitive scalar; appends the
/// matching tag(s).
fn tag_primitive(name: &str, tags: &mut Vec<&'static str>) -> bool {
    match name {
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" => {
            tags.push(v::TAG_INT);
            true
        }
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" => {
            tags.push(v::TAG_UINT);
            true
        }
        "f32" | "f64" => {
            tags.push(v::TAG_FLOAT);
            true
        }
        "bool" => {
            tags.push(v::TAG_BOOL);
            true
        }
        "char" => {
            tags.push(v::TAG_CHAR);
            true
        }
        "str" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_BORROWED);
            true
        }
        "()" => {
            tags.push(v::TAG_UNIT);
            true
        }
        _ => false,
    }
}

fn push_container_tags(tags: &mut Vec<&'static str>) {
    tags.push(v::TAG_CONTAINER);
    tags.push(v::TAG_OWNED);
    tags.push(v::TAG_DYNAMIC);
}

fn push_smart_pointer_tags(tags: &mut Vec<&'static str>) {
    tags.push(v::TAG_SMART_POINTER);
    tags.push(v::TAG_OWNED);
}

/// Convert a `syn::FnArg` (one element of `fn_sig.inputs`) into a
/// `Parameter`. `position` is 0-indexed source order; receiver (`self`,
/// `&self`, `&mut self`) is always position 0 when present.
pub(super) fn fnarg_to_parameter(arg: &FnArg, position: u32) -> Parameter {
    match arg {
        FnArg::Receiver(r) => {
            let raw = arg.to_token_stream().to_string();
            let modifier = if r.reference.is_some() {
                if r.mutability.is_some() {
                    Some(ParamModifier::MutRef)
                } else {
                    Some(ParamModifier::Ref)
                }
            } else if r.mutability.is_some() {
                Some(ParamModifier::Move)
            } else {
                Some(ParamModifier::Own)
            };
            let mut tags = Vec::new();
            if r.reference.is_some() {
                tags.push(v::TAG_REFERENCE.to_string());
                if r.mutability.is_some() {
                    tags.push(v::TAG_MUTABLE_REF.to_string());
                }
            } else {
                tags.push(v::TAG_OWNED.to_string());
            }
            Parameter {
                position,
                name: Some("self".to_string()),
                type_raw: Some(raw.clone()),
                type_tags: tags,
                type_shape: Some(TypeShape::leaf_raw("Self", raw)),
                default_value: None,
                modifier,
                is_variadic: false,
                is_self: true,
            }
        }
        FnArg::Typed(t) => {
            let name = match &*t.pat {
                syn::Pat::Ident(pi) => Some(pi.ident.to_string()),
                syn::Pat::Wild(_) => Some("_".to_string()),
                _ => None,
            };
            let modifier = match &*t.ty {
                Type::Reference(r) if r.mutability.is_some() => Some(ParamModifier::MutRef),
                Type::Reference(_) => Some(ParamModifier::Ref),
                _ => Some(ParamModifier::Own),
            };
            Parameter {
                position,
                name,
                type_raw: Some(type_to_string(&*t.ty)),
                type_tags: type_tags_for(&t.ty),
                type_shape: Some(type_to_shape(&t.ty)),
                default_value: None,
                modifier,
                is_variadic: false,
                is_self: false,
            }
        }
    }
}

/// Convert a `syn::ReturnType` into the semantic `ReturnType`. `Default`
/// (no `-> T` annotation) is treated as `()` / `unit`.
pub(super) fn return_type_for(output: &ReturnType) -> SemReturnType {
    match output {
        ReturnType::Default => SemReturnType {
            type_raw: Some("()".to_string()),
            type_tags: vec![v::TAG_UNIT.to_string()],
            type_shape: Some(TypeShape::leaf("Unit")),
        },
        ReturnType::Type(_, ty) => SemReturnType {
            type_raw: Some(type_to_string(&**ty)),
            type_tags: type_tags_for(ty),
            type_shape: Some(type_to_shape(ty)),
        },
    }
}

/// Convert a `syn::Generics` into the semantic `GenericParam` list.
/// Lifetimes and const generics are skipped — only type parameters are
/// surfaced.
pub(super) fn generics_for(generics: &syn::Generics) -> Vec<GenericParam> {
    generics
        .params
        .iter()
        .filter_map(|p| match p {
            SynGenericParam::Type(tp) => Some(GenericParam {
                name: tp.ident.to_string(),
                bounds: tp
                    .bounds
                    .iter()
                    .map(|b| b.to_token_stream().to_string())
                    .collect(),
                default: tp.default.as_ref().map(type_to_string),
            }),
            _ => None,
        })
        .collect()
}

/// Extract the effect set for a Rust function signature + attributes.
/// `async fn` → `async`; `unsafe fn` → `unsafe`; `const fn` → `const_eval`;
/// `extern "C"` → `extern`; `#[deprecated]` → `deprecated`; `#[test]` →
/// `test`; `#[inline]` → `inline`; `#[no_mangle]` → `extern`.
///
/// Also scans the body token stream (passed in by the visitor) for panic
/// macro names so functions calling `panic!`/`unwrap`/`expect` get the
/// `may_panic` effect; callers that already compute `panic_paths` via the
/// `ComplexityVisitor` can pass the count via `body_panic_paths`.
pub(super) fn effects_for_sig(
    sig: &Signature,
    attrs: &[Attribute],
    body_panic_paths: u32,
    body_unsafe_blocks: u32,
) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    if sig.asyncness.is_some() {
        effects.push(v::EFFECT_ASYNC);
    }
    if sig.unsafety.is_some() {
        effects.push(v::EFFECT_UNSAFE);
    }
    if sig.constness.is_some() {
        effects.push(v::EFFECT_CONST_EVAL);
    }
    if sig.abi.is_some() {
        effects.push(v::EFFECT_EXTERN);
    }
    if body_panic_paths > 0 {
        effects.push(v::EFFECT_MAY_PANIC);
    }
    if body_unsafe_blocks > 0 {
        effects.push(v::EFFECT_UNSAFE);
    }
    for attr in attrs {
        let path = attr.path().to_token_stream().to_string();
        let trimmed = path.trim();
        match trimmed {
            "deprecated" => effects.push(v::EFFECT_DEPRECATED),
            "test" | "tokio :: test" | "tokio::test" => effects.push(v::EFFECT_TEST),
            "inline" => effects.push(v::EFFECT_INLINE),
            "no_mangle" => effects.push(v::EFFECT_EXTERN),
            "must_use" => {
                // `#[must_use]` is informational — surface as `pure` since the
                // caller MUST consume the value (typically a result-like).
                effects.push(v::EFFECT_PURE);
            }
            _ => {}
        }
    }
    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

/// Helper used by the visitor: derive the function body's token stream for
/// the panic-path counter (rebuilds the ComplexityVisitor inputs without
/// importing every dependency).
pub(super) fn count_body_panics(stream: &TokenStream) -> u32 {
    use proc_macro2::TokenTree;
    let mut count = 0u32;
    let mut tokens = stream.clone().into_iter().peekable();
    while let Some(tt) = tokens.next() {
        match tt {
            TokenTree::Ident(id) => {
                let name = id.to_string();
                if is_panic_ident(&name) {
                    // Look ahead for `!` (macro invocation) or `.unwrap()`-style chain.
                    if let Some(TokenTree::Punct(p)) = tokens.peek()
                        && p.as_char() == '!'
                    {
                        count = count.saturating_add(1);
                        continue;
                    }
                    if matches!(name.as_str(), "unwrap" | "expect") {
                        count = count.saturating_add(1);
                    }
                }
            }
            TokenTree::Group(g) => {
                count = count.saturating_add(count_body_panics(&g.stream()));
            }
            _ => {}
        }
    }
    count
}

fn is_panic_ident(name: &str) -> bool {
    matches!(
        name,
        "panic"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "unreachable"
            | "todo"
            | "unimplemented"
            | "unwrap"
            | "expect"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

    fn parse_type(s: &str) -> Type {
        parse_str::<Type>(s).expect("parse type")
    }

    #[test]
    fn shape_of_simple_type() {
        let s = type_to_shape(&parse_type("i32"));
        assert_eq!(s.constructor, "i32");
        assert!(s.args.is_empty());
        assert_eq!(s.raw.as_deref(), Some("i32"));
    }

    #[test]
    fn shape_of_nested_generic() {
        let s = type_to_shape(&parse_type("Vec<Result<u8, IoError>>"));
        assert_eq!(s.constructor, "Vec");
        assert_eq!(s.args.len(), 1);
        let inner = &s.args[0];
        assert_eq!(inner.constructor, "Result");
        assert_eq!(inner.args.len(), 2);
        assert_eq!(inner.args[0].constructor, "u8");
        assert_eq!(inner.args[1].constructor, "IoError");
    }

    #[test]
    fn shape_of_reference() {
        let s = type_to_shape(&parse_type("&mut Vec<u8>"));
        assert_eq!(s.constructor, "&mut");
        assert_eq!(s.args.len(), 1);
        assert_eq!(s.args[0].constructor, "Vec");
    }

    #[test]
    fn shape_of_unit_and_tuple() {
        let s = type_to_shape(&parse_type("()"));
        assert_eq!(s.constructor, "Unit");
        let s2 = type_to_shape(&parse_type("(i32, String)"));
        assert_eq!(s2.constructor, "Tuple");
        assert_eq!(s2.args.len(), 2);
    }

    #[test]
    fn tags_for_primitives() {
        assert!(type_tags_for(&parse_type("i32")).contains(&v::TAG_INT.to_string()));
        assert!(type_tags_for(&parse_type("u8")).contains(&v::TAG_UINT.to_string()));
        assert!(type_tags_for(&parse_type("bool")).contains(&v::TAG_BOOL.to_string()));
        assert!(type_tags_for(&parse_type("f64")).contains(&v::TAG_FLOAT.to_string()));
        assert!(type_tags_for(&parse_type("String")).contains(&v::TAG_STRING.to_string()));
        assert!(type_tags_for(&parse_type("String")).contains(&v::TAG_OWNED.to_string()));
        assert!(type_tags_for(&parse_type("&str")).contains(&v::TAG_BORROWED.to_string()));
        assert!(type_tags_for(&parse_type("&str")).contains(&v::TAG_REFERENCE.to_string()));
    }

    #[test]
    fn tags_for_vec_includes_container_and_indexed() {
        let tags = type_tags_for(&parse_type("Vec<u8>"));
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(tags.contains(&v::TAG_OWNED.to_string()));
        assert!(tags.contains(&v::TAG_DYNAMIC.to_string()));
        assert!(tags.contains(&v::TAG_ORDERED.to_string()));
        assert!(tags.contains(&v::TAG_UINT.to_string())); // u8 element propagates
    }

    #[test]
    fn tags_for_hashmap_marks_keyed_unordered() {
        let tags = type_tags_for(&parse_type("HashMap<String, i64>"));
        assert!(tags.contains(&v::TAG_KEYED.to_string()));
        assert!(tags.contains(&v::TAG_UNORDERED.to_string()));
        assert!(!tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
    }

    #[test]
    fn tags_for_btreemap_marks_keyed_ordered() {
        let tags = type_tags_for(&parse_type("BTreeMap<u64, Value>"));
        assert!(tags.contains(&v::TAG_KEYED.to_string()));
        assert!(tags.contains(&v::TAG_ORDERED.to_string()));
        assert!(!tags.contains(&v::TAG_UNORDERED.to_string()));
    }

    #[test]
    fn tags_for_option_marks_null_like() {
        let tags = type_tags_for(&parse_type("Option<Token>"));
        assert!(tags.contains(&v::TAG_OPTION.to_string()));
        assert!(tags.contains(&v::TAG_NULL_LIKE.to_string()));
    }

    #[test]
    fn tags_for_result_marks_sum_type() {
        let tags = type_tags_for(&parse_type("Result<T, E>"));
        assert!(tags.contains(&v::TAG_RESULT.to_string()));
        assert!(tags.contains(&v::TAG_SUM_TYPE.to_string()));
    }

    #[test]
    fn tags_for_arc_mutex_marks_concurrency() {
        let tags = type_tags_for(&parse_type("Arc<Mutex<HashMap<String, i64>>>"));
        assert!(tags.contains(&v::TAG_SMART_POINTER.to_string()));
        assert!(tags.contains(&v::TAG_SHARED.to_string()));
        assert!(tags.contains(&v::TAG_CONCURRENCY.to_string()));
    }

    #[test]
    fn tags_for_pin_box_future_marks_future() {
        let tags = type_tags_for(&parse_type("Pin<Box<Future<Output=()>>>"));
        assert!(
            tags.contains(&v::TAG_FUTURE.to_string()) || tags.contains(&v::TAG_ASYNC.to_string())
        );
    }

    #[test]
    fn tags_for_box_marks_unique_owned() {
        let tags = type_tags_for(&parse_type("Box<dyn Trait>"));
        assert!(tags.contains(&v::TAG_SMART_POINTER.to_string()));
        assert!(tags.contains(&v::TAG_UNIQUE.to_string()));
    }

    #[test]
    fn tags_for_dyn_trait_marks_existential_interface() {
        let tags = type_tags_for(&parse_type("dyn std::error::Error + Send"));
        assert!(tags.contains(&v::TAG_EXISTENTIAL.to_string()));
        assert!(tags.contains(&v::TAG_INTERFACE.to_string()));
    }

    #[test]
    fn fnarg_receiver_self_is_self() {
        let f: syn::ItemFn = parse_str("fn f(&self) {}").expect("parse");
        let arg = f.sig.inputs.first().expect("receiver");
        let p = fnarg_to_parameter(arg, 0);
        assert!(p.is_self);
        assert_eq!(p.name.as_deref(), Some("self"));
        assert_eq!(p.modifier, Some(ParamModifier::Ref));
    }

    #[test]
    fn fnarg_typed_carries_name_and_modifier() {
        let f: syn::ItemFn = parse_str("fn f(x: &mut Vec<u8>) {}").expect("parse");
        let arg = f.sig.inputs.first().expect("typed");
        let p = fnarg_to_parameter(arg, 0);
        assert!(!p.is_self);
        assert_eq!(p.name.as_deref(), Some("x"));
        assert_eq!(p.modifier, Some(ParamModifier::MutRef));
        assert!(p.type_tags.contains(&v::TAG_MUTABLE_REF.to_string()));
        assert!(p.type_tags.contains(&v::TAG_CONTAINER.to_string()));
    }

    #[test]
    fn return_default_is_unit() {
        let f: syn::ItemFn = parse_str("fn f() {}").expect("parse");
        let rt = return_type_for(&f.sig.output);
        assert_eq!(rt.type_raw.as_deref(), Some("()"));
        assert!(rt.type_tags.contains(&v::TAG_UNIT.to_string()));
    }

    #[test]
    fn return_typed_carries_tags_and_shape() {
        let f: syn::ItemFn =
            parse_str("fn f() -> Result<Token, AuthError> { todo!() }").expect("parse");
        let rt = return_type_for(&f.sig.output);
        assert!(rt.type_tags.contains(&v::TAG_RESULT.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_SUM_TYPE.to_string()));
        let shape = rt.type_shape.expect("shape");
        assert_eq!(shape.constructor, "Result");
        assert_eq!(shape.args.len(), 2);
    }

    #[test]
    fn generics_extracted_with_bounds() {
        let f: syn::ItemFn = parse_str("fn f<T: Clone + Send, U>(x: T, y: U) {}").expect("parse");
        let gs = generics_for(&f.sig.generics);
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0].name, "T");
        assert!(gs[0].bounds.iter().any(|b| b.contains("Clone")));
        assert!(gs[0].bounds.iter().any(|b| b.contains("Send")));
        assert_eq!(gs[1].name, "U");
        assert!(gs[1].bounds.is_empty());
    }

    #[test]
    fn generics_with_default_type() {
        let f: syn::ItemFn = parse_str("fn f<T = String>() {}").expect("parse");
        let gs = generics_for(&f.sig.generics);
        assert_eq!(gs[0].default.as_deref().map(str::trim), Some("String"));
    }

    #[test]
    fn effects_for_async_fn() {
        let f: syn::ItemFn = parse_str("async fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_ASYNC.to_string()));
        assert!(!effs.contains(&v::EFFECT_UNSAFE.to_string()));
    }

    #[test]
    fn effects_for_unsafe_fn() {
        let f: syn::ItemFn = parse_str("unsafe fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_UNSAFE.to_string()));
    }

    #[test]
    fn effects_for_const_fn() {
        let f: syn::ItemFn = parse_str("const fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_CONST_EVAL.to_string()));
    }

    #[test]
    fn effects_for_extern_fn() {
        let f: syn::ItemFn = parse_str(r#"extern "C" fn f() {}"#).expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_EXTERN.to_string()));
    }

    #[test]
    fn effects_for_deprecated_attr() {
        let f: syn::ItemFn = parse_str("#[deprecated] fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_DEPRECATED.to_string()));
    }

    #[test]
    fn effects_for_test_attr() {
        let f: syn::ItemFn = parse_str("#[test] fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_TEST.to_string()));
    }

    #[test]
    fn effects_for_inline_attr() {
        let f: syn::ItemFn = parse_str("#[inline] fn f() {}").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 0);
        assert!(effs.contains(&v::EFFECT_INLINE.to_string()));
    }

    #[test]
    fn effects_for_panicking_body() {
        let f: syn::ItemFn = parse_str("fn f() { panic!(\"oops\"); }").expect("parse");
        let panics = count_body_panics(&f.block.to_token_stream());
        assert_eq!(panics, 1);
        let effs = effects_for_sig(&f.sig, &f.attrs, panics, 0);
        assert!(effs.contains(&v::EFFECT_MAY_PANIC.to_string()));
    }

    #[test]
    fn effects_for_unwrap_chain() {
        let f: syn::ItemFn =
            parse_str("fn f(x: Option<i32>) -> i32 { x.unwrap() }").expect("parse");
        let panics = count_body_panics(&f.block.to_token_stream());
        assert!(
            panics >= 1,
            "expected unwrap to count as panic; got {panics}"
        );
    }

    #[test]
    fn effects_deduplicate() {
        // unsafe fn that ALSO has an unsafe block — only one entry.
        let f: syn::ItemFn = parse_str("unsafe fn f() { unsafe { } }").expect("parse");
        let effs = effects_for_sig(&f.sig, &f.attrs, 0, 1);
        let unsafe_count = effs
            .iter()
            .filter(|e| e.as_str() == v::EFFECT_UNSAFE)
            .count();
        assert_eq!(unsafe_count, 1);
    }
}
