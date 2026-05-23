//! Map tree-sitter-python nodes to shadow-ASR semantic structures
//! (`Parameter`, `ReturnType`, `GenericParam`, type tags, effects).
//!
//! Python's type system is gradual: type hints are optional. When the
//! source supplies them (`def f(x: int) -> bool`), the mapper produces
//! richer tags and shape; when they're absent, the parameter row carries
//! `type_raw = None` and an empty `type_tags`, which is still useful for
//! cross-language signature shape matching by parameter count.

use tree_sitter::Node;

use crate::parsing::symbols::{
    GenericParam, ParamModifier, Parameter, ReturnType as SemReturnType,
};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Convert a tree-sitter-python `type` node (the annotation in
/// `param: type` or `def f() -> type:`) into a `TypeShape`.
pub(super) fn type_to_shape(node: Node<'_>, src: &str) -> TypeShape {
    let raw = node_text(node, src).to_string();
    type_to_shape_inner(node, src, raw)
}

fn type_to_shape_inner(node: Node<'_>, src: &str, raw: String) -> TypeShape {
    let kind = node.kind();
    // Unwrap a `type` wrapper node — annotations come wrapped as
    // `type(<inner>)`. After unwrapping, the inner node is what we
    // structurally inspect.
    let inner = if kind == "type" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    };
    let inner_kind = inner.kind();
    match inner_kind {
        "identifier" => TypeShape::leaf_raw(node_text(inner, src), raw),
        "none" => TypeShape::leaf_raw("None", raw),
        "generic_type" => {
            // Type-annotation generic: `generic_type(identifier, type_parameter(type(...), ...))`.
            let constructor = inner
                .named_child(0)
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            // The second named child is the `type_parameter` node.
            if let Some(type_param) = inner.named_child(1) {
                for tp_child in type_param.named_children(&mut type_param.walk()) {
                    args.push(type_to_shape_inner(
                        tp_child,
                        src,
                        node_text(tp_child, src).to_string(),
                    ));
                }
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        "subscript" => {
            // Expression-context subscript (e.g. `Generic[T, U]` as a base
            // class, or annotation written as expression).
            let value = inner
                .child_by_field_name("value")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            // tree-sitter-python emits repeated `subscript:` fields. Walk
            // each named child via cursor index so we can correctly query
            // its role.
            collect_subscript_field_args(inner, src, &mut args);
            TypeShape::applied_raw(value, args, raw)
        }
        "attribute" => {
            // `typing.List` → constructor = last identifier ("List").
            let attr = inner
                .child_by_field_name("attribute")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_else(|| node_text(inner, src).to_string());
            TypeShape::leaf_raw(attr, raw)
        }
        "binary_operator" => {
            // PEP 604: `int | None` is a union type.
            let mut args: Vec<TypeShape> = Vec::new();
            for child in inner.named_children(&mut inner.walk()) {
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            TypeShape::applied_raw("Union", args, raw)
        }
        "string" => TypeShape::leaf_raw("ForwardRef", raw),
        "list" => {
            // `[int, str]` literal-as-type (rare but legal).
            let mut args: Vec<TypeShape> = Vec::new();
            for child in inner.named_children(&mut inner.walk()) {
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            TypeShape::applied_raw("List", args, raw)
        }
        _ => TypeShape::leaf_raw(node_text(inner, src), raw),
    }
}

/// Map a Python type annotation node to its canonical tag set.
pub(super) fn type_tags_for(node: Node<'_>, src: &str) -> Vec<String> {
    let mut tags: Vec<&'static str> = Vec::new();
    populate_tags(node, src, &mut tags);
    let mut owned: Vec<String> = tags.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

fn populate_tags(node: Node<'_>, src: &str, tags: &mut Vec<&'static str>) {
    let kind = node.kind();
    let inner = if kind == "type" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    };
    let inner_kind = inner.kind();
    match inner_kind {
        "identifier" => {
            tag_constructor(node_text(inner, src), tags);
        }
        "none" => tags.push(v::TAG_NULL_LIKE),
        "generic_type" => {
            // `generic_type(identifier, type_parameter(type(...), ...))`.
            let value = inner
                .named_child(0)
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&value, tags);
            // Recurse into the type_parameter's inner `type` children.
            if let Some(type_param) = inner.named_child(1) {
                for tp_child in type_param.named_children(&mut type_param.walk()) {
                    let mut inner_tags: Vec<&'static str> = Vec::new();
                    populate_tags(tp_child, src, &mut inner_tags);
                    tags.extend(inner_tags);
                }
            }
        }
        "subscript" => {
            let value = inner
                .child_by_field_name("value")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&value, tags);
            // Walk repeated `subscript:` field children.
            collect_subscript_field_tags(inner, src, tags);
        }
        "attribute" => {
            let attr = inner
                .child_by_field_name("attribute")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_else(|| node_text(inner, src).to_string());
            tag_constructor(&attr, tags);
        }
        "binary_operator" => {
            // PEP 604: union types.
            tags.push(v::TAG_UNION);
            tags.push(v::TAG_SUM_TYPE);
            // Special-case `T | None` → option.
            let mut has_none = false;
            for child in inner.named_children(&mut inner.walk()) {
                if child.kind() == "none" {
                    has_none = true;
                }
            }
            if has_none {
                tags.push(v::TAG_OPTION);
                tags.push(v::TAG_NULL_LIKE);
            }
        }
        _ => tags.push(v::TAG_UNKNOWN),
    }
}

/// Tag a Python constructor name. Returns `true` if matched, `false` for
/// unknown (caller may still want to emit `TAG_UNKNOWN`).
fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) -> bool {
    if tag_primitive(name, tags) {
        return true;
    }
    match name {
        // ── Container shape ───────────────────────────────────────────
        "list" | "List" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
            true
        }
        "tuple" | "Tuple" => {
            tags.push(v::TAG_PRODUCT_TYPE);
            tags.push(v::TAG_FIXED_SIZE);
            tags.push(v::TAG_OWNED);
            true
        }
        "dict" | "Dict" | "Mapping" | "MutableMapping" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            true
        }
        "OrderedDict" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_ORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            true
        }
        "set" | "Set" | "MutableSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            true
        }
        "frozenset" | "FrozenSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            true
        }
        "deque" | "Deque" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            true
        }
        // ── Algebraic ───────────────────────────────────────────────
        "Optional" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
            true
        }
        "Union" => {
            tags.push(v::TAG_UNION);
            tags.push(v::TAG_SUM_TYPE);
            true
        }
        // ── Computation ─────────────────────────────────────────────
        "Awaitable" | "Coroutine" | "Future" | "AsyncIterator" | "AsyncGenerator" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
            true
        }
        "Iterable" | "Iterator" => {
            tags.push(v::TAG_ITERATOR);
            true
        }
        "Generator" => {
            tags.push(v::TAG_GENERATOR);
            tags.push(v::TAG_ITERATOR);
            true
        }
        "Callable" => {
            tags.push(v::TAG_FUNCTION);
            true
        }
        // ── Special ─────────────────────────────────────────────────
        "Any" => {
            tags.push(v::TAG_UNKNOWN);
            true
        }
        "Never" | "NoReturn" => {
            tags.push(v::TAG_NEVER);
            true
        }
        "TypeVar" => {
            tags.push(v::TAG_TYPE_PARAMETER);
            true
        }
        // ── Concurrency / IO ────────────────────────────────────────
        "Lock" | "RLock" | "Semaphore" => {
            tags.push(v::TAG_MUTEX);
            tags.push(v::TAG_CONCURRENCY);
            true
        }
        "Queue" | "asyncio.Queue" => {
            tags.push(v::TAG_CHANNEL);
            tags.push(v::TAG_CONCURRENCY);
            true
        }
        "Path" | "PurePath" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_FILESYSTEM);
            true
        }
        _ => false,
    }
}

/// Walk a `subscript` node's children via cursor and collect those whose
/// role is `subscript` into `args` as `TypeShape` rows.
fn collect_subscript_field_args(node: Node<'_>, src: &str, args: &mut Vec<TypeShape>) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() && cursor.field_name() == Some("subscript") {
                let child = cursor.node();
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Walk a `subscript` node's children via cursor and populate tag rows
/// for those whose role is `subscript`.
fn collect_subscript_field_tags(node: Node<'_>, src: &str, tags: &mut Vec<&'static str>) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() && cursor.field_name() == Some("subscript") {
                let child = cursor.node();
                let mut inner_tags: Vec<&'static str> = Vec::new();
                populate_tags(child, src, &mut inner_tags);
                tags.extend(inner_tags);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn tag_primitive(name: &str, tags: &mut Vec<&'static str>) -> bool {
    match name {
        "int" => {
            tags.push(v::TAG_INT);
            true
        }
        "float" => {
            tags.push(v::TAG_FLOAT);
            true
        }
        "bool" => {
            tags.push(v::TAG_BOOL);
            true
        }
        "str" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
            true
        }
        "bytes" => {
            tags.push(v::TAG_BYTES);
            true
        }
        "complex" => {
            tags.push(v::TAG_FLOAT);
            true
        }
        "None" => {
            tags.push(v::TAG_NULL_LIKE);
            true
        }
        _ => false,
    }
}

/// Build the `Parameter` list from a `parameters` tree-sitter node.
/// Walks `parameters` children (`identifier`, `typed_parameter`,
/// `typed_default_parameter`, `default_parameter`,
/// `list_splat_pattern` for `*args`, `dictionary_splat_pattern` for
/// `**kwargs`). The implicit `self` / `cls` receiver is flagged via
/// `is_self`.
pub(super) fn parameters_from_node(parameters_node: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut position: u32 = 0;
    let mut kwonly_phase = false; // after `*` or `*args`, args are keyword-only.
    let mut saw_positional_separator = false; // PEP 570 — `/` already seen.

    // Walk every named child of `parameters`.
    for child in parameters_node.named_children(&mut parameters_node.walk()) {
        let kind = child.kind();
        // Marker separators flip the phase flags but don't emit a row.
        match kind {
            "positional_separator" => {
                saw_positional_separator = true;
                continue;
            }
            "keyword_separator" => {
                kwonly_phase = true;
                continue;
            }
            _ => {}
        }
        if let Some(p) = parameter_from_node(child, src, position, kwonly_phase) {
            // PEP 570 positional-only applies to plain params *before* `/`.
            // Variadic params (`*args` / `**kwargs`) keep their own modifier.
            let is_variadic_kind =
                matches!(kind, "list_splat_pattern" | "dictionary_splat_pattern");
            let p = if !saw_positional_separator
                && !is_variadic_kind
                && !kwonly_phase
                && position > 0
            {
                // Pre-`/` plain params (excluding receiver at position 0) are
                // candidates for PosOnly — but only emit it once we've SEEN
                // a `/` separator. Without `/`, params are regular positional.
                p
            } else {
                p
            };
            // If `/` was seen and we're still pre-`*`, anything before the
            // separator was positional-only (we re-walk to retroactively
            // mark; cheap because parameter list is short).
            let p = if saw_positional_separator && !is_variadic_kind && !kwonly_phase {
                // After `/`, the params become regular again. Leave as-is.
                p
            } else {
                p
            };
            // First parameter named `self` or `cls` is the implicit receiver.
            let p = if position == 0 && matches!(p.name.as_deref(), Some("self") | Some("cls")) {
                Parameter { is_self: true, ..p }
            } else {
                p
            };
            // After `*args`, subsequent params are keyword-only.
            if kind == "list_splat_pattern" {
                kwonly_phase = true;
            }
            out.push(p);
            position += 1;
        }
    }
    // PEP 570 retro-fit: if `/` was seen, mark every plain param before the
    // first separator as PosOnly.
    if saw_positional_separator {
        // The `positional_separator` index in source order — count how many
        // plain params preceded it. We can't easily recover the index post-hoc
        // here, so walk parameters_node again and stop at the separator.
        let mut idx: usize = 0;
        let mut posonly_count: usize = 0;
        for child in parameters_node.named_children(&mut parameters_node.walk()) {
            if child.kind() == "positional_separator" {
                posonly_count = idx;
                break;
            }
            if !matches!(child.kind(), "keyword_separator") {
                idx += 1;
            }
        }
        for p in out.iter_mut().take(posonly_count) {
            if !p.is_self && !p.is_variadic {
                p.modifier = Some(ParamModifier::PosOnly);
            }
        }
    }
    out
}

fn parameter_from_node(
    node: Node<'_>,
    src: &str,
    position: u32,
    kwonly_phase: bool,
) -> Option<Parameter> {
    let kind = node.kind();
    let modifier = if kwonly_phase {
        Some(ParamModifier::KwOnly)
    } else {
        Some(ParamModifier::Own)
    };
    match kind {
        "identifier" => Some(Parameter {
            position,
            name: Some(node_text(node, src).to_string()),
            type_raw: None,
            type_tags: Vec::new(),
            type_shape: None,
            default_value: None,
            modifier,
            is_variadic: false,
            is_self: false,
        }),
        "typed_parameter" => {
            let name = node
                .named_child(0)
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(n, src).to_string());
            let ty = node.child_by_field_name("type");
            let (type_raw, type_tags, type_shape) = if let Some(t) = ty {
                (
                    Some(node_text(t, src).to_string()),
                    type_tags_for(t, src),
                    Some(type_to_shape(t, src)),
                )
            } else {
                (None, Vec::new(), None)
            };
            Some(Parameter {
                position,
                name,
                type_raw,
                type_tags,
                type_shape,
                default_value: None,
                modifier,
                is_variadic: false,
                is_self: false,
            })
        }
        "default_parameter" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string());
            let default_value = node
                .child_by_field_name("value")
                .map(|n| node_text(n, src).to_string());
            Some(Parameter {
                position,
                name,
                type_raw: None,
                type_tags: Vec::new(),
                type_shape: None,
                default_value,
                modifier,
                is_variadic: false,
                is_self: false,
            })
        }
        "typed_default_parameter" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string());
            let ty = node.child_by_field_name("type");
            let (type_raw, type_tags, type_shape) = if let Some(t) = ty {
                (
                    Some(node_text(t, src).to_string()),
                    type_tags_for(t, src),
                    Some(type_to_shape(t, src)),
                )
            } else {
                (None, Vec::new(), None)
            };
            let default_value = node
                .child_by_field_name("value")
                .map(|n| node_text(n, src).to_string());
            Some(Parameter {
                position,
                name,
                type_raw,
                type_tags,
                type_shape,
                default_value,
                modifier,
                is_variadic: false,
                is_self: false,
            })
        }
        "list_splat_pattern" => {
            // `*args` — variadic positional.
            let name = node
                .named_child(0)
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(n, src).to_string());
            Some(Parameter {
                position,
                name,
                type_raw: None,
                type_tags: Vec::new(),
                type_shape: None,
                default_value: None,
                modifier,
                is_variadic: true,
                is_self: false,
            })
        }
        "dictionary_splat_pattern" => {
            // `**kwargs` — variadic keyword.
            let name = node
                .named_child(0)
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(n, src).to_string());
            Some(Parameter {
                position,
                name,
                type_raw: None,
                type_tags: Vec::new(),
                type_shape: None,
                default_value: None,
                modifier: Some(ParamModifier::KwOnly),
                is_variadic: true,
                is_self: false,
            })
        }
        _ => None,
    }
}

/// Build the return-type structure from the function definition's
/// `return_type` field. `None` argument → return `()` (the unit row).
pub(super) fn return_type_from_node(node: Option<Node<'_>>, src: &str) -> SemReturnType {
    match node {
        None => SemReturnType {
            type_raw: Some("None".to_string()),
            type_tags: vec![v::TAG_UNIT.to_string(), v::TAG_NULL_LIKE.to_string()],
            type_shape: Some(TypeShape::leaf("None")),
        },
        Some(t) => SemReturnType {
            type_raw: Some(node_text(t, src).to_string()),
            type_tags: type_tags_for(t, src),
            type_shape: Some(type_to_shape(t, src)),
        },
    }
}

/// Extract effects from a Python function definition node + its
/// decorators. Decorators are tree-sitter `decorator` nodes immediately
/// preceding the function definition (parent walk: traverse
/// `decorated_definition`).
///
/// Recognized effects:
/// - `async def f(...)` → `async`
/// - `@deprecated` (or `@warnings.deprecated`) → `deprecated`
/// - `@property` → `pure` (typically pure getter)
/// - `@staticmethod` / `@classmethod` → no effect (modifier handled in params)
/// - `@pytest.fixture` / `@pytest.mark.parametrize` → `test` (best-effort)
/// - Function name starting with `test_` and inside a test file → `test`
///   (heuristic, applied when in test directories)
pub(super) fn effects_for_function(node: Node<'_>, src: &str) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();

    // `async def` — the function_definition's first child is `async` when present.
    for child in node.children(&mut node.walk()) {
        if child.kind() == "async" {
            effects.push(v::EFFECT_ASYNC);
            break;
        }
    }

    // Decorators live on the parent `decorated_definition` node.
    if let Some(parent) = node.parent()
        && parent.kind() == "decorated_definition"
    {
        for sib in parent.children(&mut parent.walk()) {
            if sib.kind() != "decorator" {
                continue;
            }
            let raw = node_text(sib, src);
            // Strip leading `@` and trailing parens/args.
            let name_part = raw
                .trim_start_matches('@')
                .split_once('(')
                .map(|(n, _)| n)
                .unwrap_or_else(|| raw.trim_start_matches('@').trim());
            let last = name_part.rsplit('.').next().unwrap_or(name_part).trim();
            match last {
                "deprecated" => effects.push(v::EFFECT_DEPRECATED),
                "property" | "cached_property" => effects.push(v::EFFECT_PURE),
                "fixture" | "mark" | "parametrize" | "skip" | "skipif" | "xfail" => {
                    effects.push(v::EFFECT_TEST)
                }
                _ => {}
            }
        }
    }

    // Heuristic: name starts with `test_` → likely a test function.
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = node_text(name_node, src);
        if name.starts_with("test_") {
            effects.push(v::EFFECT_TEST);
        }
    }

    // Scan the function body for raise statements → `throws`.
    if let Some(body) = node.child_by_field_name("body") {
        if body_contains_raise(body) {
            effects.push(v::EFFECT_THROWS);
        }
        if body_calls_assert(body, src) {
            effects.push(v::EFFECT_MAY_PANIC);
        }
    }

    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

fn body_contains_raise(node: Node<'_>) -> bool {
    let kind = node.kind();
    if kind == "raise_statement" {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if body_contains_raise(child) {
            return true;
        }
    }
    false
}

fn body_calls_assert(node: Node<'_>, src: &str) -> bool {
    let kind = node.kind();
    if kind == "assert_statement" {
        return true;
    }
    if kind == "call"
        && let Some(func) = node.child_by_field_name("function")
        && node_text(func, src) == "assert"
    {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if body_calls_assert(child, src) {
            return true;
        }
    }
    false
}

/// Generic parameters in Python are surfaced via `Generic[T, U]` in the
/// class base list or via standalone `TypeVar` definitions. The simple
/// case — `class Foo(Generic[T, U]):` — is handled here; full TypeVar
/// resolution would require cross-statement analysis and is out of
/// scope for the first pass.
pub(super) fn generics_for_class(node: Node<'_>, src: &str) -> Vec<GenericParam> {
    let mut out: Vec<GenericParam> = Vec::new();
    let Some(superclasses) = node.child_by_field_name("superclasses") else {
        return out;
    };
    let mut sc_cursor = superclasses.walk();
    for child in superclasses.named_children(&mut sc_cursor) {
        if child.kind() != "subscript" {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let value_name = node_text(value, src);
        if value_name != "Generic" && !value_name.ends_with(".Generic") {
            continue;
        }
        // The subscript node holds repeated `subscript:` fields. Walk via
        // cursor to read each one's role correctly.
        let mut inner_cursor = child.walk();
        if inner_cursor.goto_first_child() {
            loop {
                if inner_cursor.node().is_named() && inner_cursor.field_name() == Some("subscript")
                {
                    let arg = inner_cursor.node();
                    if arg.kind() == "identifier" {
                        out.push(GenericParam {
                            name: node_text(arg, src).to_string(),
                            bounds: Vec::new(),
                            default: None,
                        });
                    }
                }
                if !inner_cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_python(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("set_language python");
        p.parse(src, None).expect("parse")
    }

    fn first_function<'a>(tree: &'a tree_sitter::Tree, kind: &str) -> Option<Node<'a>> {
        // Walk the tree for the first node of the given kind.
        fn descend<'a>(node: Node<'a>, target: &str) -> Option<Node<'a>> {
            if node.kind() == target {
                return Some(node);
            }
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                if let Some(found) = descend(child, target) {
                    return Some(found);
                }
            }
            None
        }
        descend(tree.root_node(), kind)
    }

    #[test]
    fn tags_for_python_primitives() {
        let src = "def f(x: int, y: str, z: bool, w: float, b: bytes) -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 5);
        assert!(parsed[0].type_tags.contains(&v::TAG_INT.to_string()));
        assert!(parsed[1].type_tags.contains(&v::TAG_STRING.to_string()));
        assert!(parsed[2].type_tags.contains(&v::TAG_BOOL.to_string()));
        assert!(parsed[3].type_tags.contains(&v::TAG_FLOAT.to_string()));
        assert!(parsed[4].type_tags.contains(&v::TAG_BYTES.to_string()));
    }

    #[test]
    fn tags_for_list_subscript() {
        let src = "def f(xs: list[int]) -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(tags.contains(&v::TAG_OWNED.to_string()));
        // Element type bleeds through.
        assert!(tags.contains(&v::TAG_INT.to_string()));
    }

    #[test]
    fn tags_for_optional_union() {
        let src = "def f(x: int | None) -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_OPTION.to_string()));
        assert!(tags.contains(&v::TAG_NULL_LIKE.to_string()));
    }

    #[test]
    fn tags_for_dict_with_str_keys() {
        let src = "def f(m: dict[str, int]) -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(tags.contains(&v::TAG_KEYED.to_string()));
        assert!(tags.contains(&v::TAG_UNORDERED.to_string()));
    }

    #[test]
    fn return_type_default_is_none() {
        let src = "def f(): pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let rt = return_type_from_node(fn_node.child_by_field_name("return_type"), src);
        assert_eq!(rt.type_raw.as_deref(), Some("None"));
        assert!(rt.type_tags.contains(&v::TAG_UNIT.to_string()));
    }

    #[test]
    fn return_type_with_annotation() {
        let src = "def f() -> list[int]: return []";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let rt = return_type_from_node(fn_node.child_by_field_name("return_type"), src);
        assert!(rt.type_tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_INT.to_string()));
        let shape = rt.type_shape.expect("shape");
        assert_eq!(shape.constructor, "list");
        assert_eq!(shape.args.len(), 1);
    }

    #[test]
    fn effects_for_async_def() {
        let src = "async def f(): pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_ASYNC.to_string()));
    }

    #[test]
    fn effects_for_test_prefix() {
        let src = "def test_foo(): pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_TEST.to_string()));
    }

    #[test]
    fn effects_for_raise() {
        let src = "def f(): raise ValueError('bad')";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_THROWS.to_string()));
    }

    #[test]
    fn effects_for_assert() {
        let src = "def f(x: int) -> None:\n    assert x > 0\n";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_MAY_PANIC.to_string()));
    }

    #[test]
    fn self_param_marked_is_self() {
        let src = "class Foo:\n    def method(self, x: int) -> bool:\n        return True\n";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_self);
        assert_eq!(parsed[0].name.as_deref(), Some("self"));
        assert!(!parsed[1].is_self);
    }

    #[test]
    fn variadic_args_marked() {
        let src = "def f(*args, **kwargs): pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_variadic);
        assert_eq!(parsed[0].name.as_deref(), Some("args"));
        assert!(parsed[1].is_variadic);
        assert_eq!(parsed[1].name.as_deref(), Some("kwargs"));
        assert_eq!(parsed[1].modifier, Some(ParamModifier::KwOnly));
    }

    #[test]
    fn default_value_captured() {
        let src = "def f(x: int = 5, name: str = 'hi') -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].default_value.as_deref(), Some("5"));
        assert_eq!(parsed[1].default_value.as_deref(), Some("'hi'"));
    }

    #[test]
    fn shape_of_subscripted_type() {
        let src = "def f(d: dict[str, list[int]]) -> None: pass";
        let tree = parse_python(src);
        let fn_node = first_function(&tree, "function_definition").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let shape = parsed[0].type_shape.as_ref().expect("shape");
        assert_eq!(shape.constructor, "dict");
        assert_eq!(shape.args.len(), 2);
        assert_eq!(shape.args[0].constructor, "str");
        assert_eq!(shape.args[1].constructor, "list");
    }

    #[test]
    fn generics_from_class_definition() {
        let src = "from typing import Generic, TypeVar\nT = TypeVar('T')\nU = TypeVar('U')\nclass Cont(Generic[T, U]):\n    pass\n";
        let tree = parse_python(src);
        let class_node = first_function(&tree, "class_definition").expect("class");
        let generics = generics_for_class(class_node, src);
        assert_eq!(generics.len(), 2);
        assert_eq!(generics[0].name, "T");
        assert_eq!(generics[1].name, "U");
    }
}
