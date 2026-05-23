//! Shadow-ASR extraction for JavaScript / TypeScript / TSX.
//!
//! TS source surfaces real type annotations (`number`, `string`, `Array<T>`,
//! `Promise<T>`, `T | undefined`, …). JS source has none — the mapper
//! returns empty `type_raw` and an empty tag set for plain JS, which still
//! lets cross-language shape matching work by parameter count.

use tree_sitter::Node;

use crate::parsing::symbols::{ParamModifier, Parameter, ReturnType as SemReturnType};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Convert a TS type expression (the inner node inside `type_annotation`)
/// into a `TypeShape`.
pub(super) fn type_to_shape(node: Node<'_>, src: &str) -> TypeShape {
    let raw = node_text(node, src).trim().to_string();
    type_to_shape_inner(node, src, raw)
}

fn type_to_shape_inner(node: Node<'_>, src: &str, raw: String) -> TypeShape {
    let kind = node.kind();
    // Unwrap a type_annotation wrapper. The annotation child is the actual type.
    let inner = if kind == "type_annotation" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    };
    let inner_kind = inner.kind();
    match inner_kind {
        "predefined_type" | "type_identifier" => TypeShape::leaf_raw(node_text(inner, src), raw),
        "generic_type" => {
            let constructor = inner
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            if let Some(targs) = inner.child_by_field_name("type_arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    args.push(type_to_shape_inner(
                        child,
                        src,
                        node_text(child, src).to_string(),
                    ));
                }
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        "array_type" => {
            let mut cursor = inner.walk();
            let elem = inner
                .named_children(&mut cursor)
                .next()
                .map(|n| type_to_shape_inner(n, src, node_text(n, src).to_string()));
            TypeShape::applied_raw("Array", elem.map(|e| vec![e]).unwrap_or_default(), raw)
        }
        "tuple_type" => {
            let mut cursor = inner.walk();
            let mut args = Vec::new();
            for child in inner.named_children(&mut cursor) {
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            TypeShape::applied_raw("Tuple", args, raw)
        }
        "union_type" => {
            let mut cursor = inner.walk();
            let mut args = Vec::new();
            for child in inner.named_children(&mut cursor) {
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            TypeShape::applied_raw("Union", args, raw)
        }
        "intersection_type" => {
            let mut cursor = inner.walk();
            let mut args = Vec::new();
            for child in inner.named_children(&mut cursor) {
                args.push(type_to_shape_inner(
                    child,
                    src,
                    node_text(child, src).to_string(),
                ));
            }
            TypeShape::applied_raw("Intersection", args, raw)
        }
        "literal_type" => TypeShape::leaf_raw("Literal", raw),
        "object_type" => TypeShape::leaf_raw("Object", raw),
        "function_type" => TypeShape::leaf_raw("Function", raw),
        "any" => TypeShape::leaf_raw("any", raw),
        "void_type" => TypeShape::leaf_raw("void", raw),
        "never_type" => TypeShape::leaf_raw("never", raw),
        "undefined" => TypeShape::leaf_raw("undefined", raw),
        "null" => TypeShape::leaf_raw("null", raw),
        _ => TypeShape::leaf_raw(node_text(inner, src).trim(), raw),
    }
}

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
    let inner = if kind == "type_annotation" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    };
    let inner_kind = inner.kind();
    match inner_kind {
        "predefined_type" => {
            tag_primitive(node_text(inner, src), tags);
        }
        "type_identifier" => tag_constructor(node_text(inner, src), tags),
        "generic_type" => {
            let constructor = inner
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&constructor, tags);
            // Recurse into type_arguments.
            if let Some(targs) = inner.child_by_field_name("type_arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    let mut inner_tags: Vec<&'static str> = Vec::new();
                    populate_tags(child, src, &mut inner_tags);
                    tags.extend(inner_tags);
                }
            }
        }
        "array_type" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
            // Element type bleed-through.
            let mut cursor = inner.walk();
            if let Some(elem) = inner.named_children(&mut cursor).next() {
                let mut elem_tags: Vec<&'static str> = Vec::new();
                populate_tags(elem, src, &mut elem_tags);
                tags.extend(elem_tags);
            }
        }
        "tuple_type" => {
            tags.push(v::TAG_PRODUCT_TYPE);
            tags.push(v::TAG_FIXED_SIZE);
        }
        "union_type" => {
            tags.push(v::TAG_UNION);
            tags.push(v::TAG_SUM_TYPE);
            // Detect `T | undefined` / `T | null` → option.
            let mut cursor = inner.walk();
            let mut has_nullish = false;
            for child in inner.named_children(&mut cursor) {
                let txt = node_text(child, src).trim();
                if txt == "undefined" || txt == "null" {
                    has_nullish = true;
                }
            }
            if has_nullish {
                tags.push(v::TAG_OPTION);
                tags.push(v::TAG_NULL_LIKE);
            }
        }
        "intersection_type" => {
            tags.push(v::TAG_PRODUCT_TYPE);
        }
        "function_type" => tags.push(v::TAG_FUNCTION),
        "object_type" => tags.push(v::TAG_STRUCT),
        "any" => tags.push(v::TAG_UNKNOWN),
        "void_type" => tags.push(v::TAG_UNIT),
        "never_type" => tags.push(v::TAG_NEVER),
        "undefined" | "null" => tags.push(v::TAG_NULL_LIKE),
        _ => tags.push(v::TAG_UNKNOWN),
    }
}

fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    if tag_primitive(name, tags) {
        return;
    }
    match name {
        "Array" | "ReadonlyArray" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
        }
        "Map" | "WeakMap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "WeakMap" {
                tags.push(v::TAG_WEAK);
            }
        }
        "Set" | "WeakSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "WeakSet" {
                tags.push(v::TAG_WEAK);
            }
        }
        "Record" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
        }
        "Promise" | "PromiseLike" | "Awaited" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
        }
        "AsyncIterable" | "AsyncIterableIterator" | "AsyncGenerator" => {
            tags.push(v::TAG_STREAM);
            tags.push(v::TAG_ASYNC);
            tags.push(v::TAG_ITERATOR);
        }
        "Iterable" | "IterableIterator" | "Iterator" => {
            tags.push(v::TAG_ITERATOR);
        }
        "Generator" => {
            tags.push(v::TAG_GENERATOR);
            tags.push(v::TAG_ITERATOR);
        }
        "Function" => tags.push(v::TAG_FUNCTION),
        "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" => {
            tags.push(v::TAG_ERROR_TYPE);
        }
        "Buffer" | "Uint8Array" | "Uint8ClampedArray" | "Uint16Array" | "Uint32Array"
        | "Int8Array" | "Int16Array" | "Int32Array" | "BigUint64Array" | "BigInt64Array" => {
            tags.push(v::TAG_BYTES);
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_INDEXED);
        }
        "Date" => tags.push(v::TAG_OPAQUE),
        "RegExp" => tags.push(v::TAG_OPAQUE),
        _ => {}
    }
}

fn tag_primitive(name: &str, tags: &mut Vec<&'static str>) -> bool {
    match name {
        "number" => {
            // TypeScript `number` covers both int and float; tag as float.
            tags.push(v::TAG_FLOAT);
            true
        }
        "bigint" => {
            tags.push(v::TAG_INT);
            true
        }
        "string" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
            true
        }
        "boolean" => {
            tags.push(v::TAG_BOOL);
            true
        }
        "symbol" => {
            tags.push(v::TAG_OPAQUE);
            true
        }
        "object" => {
            tags.push(v::TAG_STRUCT);
            true
        }
        "unknown" => {
            tags.push(v::TAG_UNKNOWN);
            true
        }
        _ => false,
    }
}

/// Build `Parameter` rows from a TS/JS `formal_parameters` node.
pub(super) fn parameters_from_node(formals: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut cursor = formals.walk();
    for (position, child) in (0_u32..).zip(formals.named_children(&mut cursor)) {
        let kind = child.kind();
        if !matches!(
            kind,
            "required_parameter" | "optional_parameter" | "rest_pattern"
        ) {
            continue;
        }

        let is_optional = kind == "optional_parameter";
        let pattern = child
            .child_by_field_name("pattern")
            .unwrap_or_else(|| child.named_child(0).unwrap_or(child));
        let is_variadic = pattern.kind() == "rest_pattern" || kind == "rest_pattern";
        let name = if is_variadic {
            // rest_pattern wraps an identifier.
            pattern
                .named_child(0)
                .or(Some(pattern))
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(n, src).to_string())
        } else if pattern.kind() == "identifier" {
            Some(node_text(pattern, src).to_string())
        } else {
            // Destructuring patterns — represent as raw text.
            Some(node_text(pattern, src).to_string())
        };

        // Type annotation, if present (TS only).
        let type_annot = child.child_by_field_name("type");
        let (type_raw, type_tags, type_shape) = match type_annot {
            Some(t) => {
                // type_annotation wraps the inner type — `(type_annotation (predefined_type))`.
                (
                    Some(
                        node_text(t, src)
                            .trim()
                            .trim_start_matches(':')
                            .trim()
                            .to_string(),
                    ),
                    type_tags_for(t, src),
                    Some(type_to_shape(t, src)),
                )
            }
            None => (None, Vec::new(), None),
        };

        // Default value, if present.
        let default_value = child
            .child_by_field_name("value")
            .map(|n| node_text(n, src).to_string());

        let modifier = if is_optional {
            Some(ParamModifier::KwOnly)
        } else {
            Some(ParamModifier::Own)
        };

        out.push(Parameter {
            position,
            name,
            type_raw,
            type_tags,
            type_shape,
            default_value,
            modifier,
            is_variadic,
            is_self: false,
        });
    }
    out
}

/// Build the return-type rows from a TS function's `return_type:
/// (type_annotation ...)` field. JS has none → `None`.
pub(super) fn return_type_from_node(node: Option<Node<'_>>, src: &str) -> SemReturnType {
    match node {
        None => SemReturnType {
            type_raw: None,
            type_tags: Vec::new(),
            type_shape: None,
        },
        Some(t) => SemReturnType {
            type_raw: Some(
                node_text(t, src)
                    .trim()
                    .trim_start_matches(':')
                    .trim()
                    .to_string(),
            ),
            type_tags: type_tags_for(t, src),
            type_shape: Some(type_to_shape(t, src)),
        },
    }
}

/// Extract function-level effects from a TS/JS function-declaration /
/// method-definition / arrow-function node. Handles `async`, `*`
/// (generator), `@deprecated` JSDoc, and test-frame heuristics.
pub(super) fn effects_for_function(node: Node<'_>, src: &str) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    // `async function` and async arrow functions surface an `async` child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "async" {
            effects.push(v::EFFECT_ASYNC);
        }
        // Generator functions are denoted by a `*` token.
        if node_text(child, src) == "*" {
            effects.push(v::EFFECT_GENERATOR);
        }
    }
    // JSDoc deprecation: scan the preceding comment block for `@deprecated`.
    // Less precise than the Python attr scan but a useful heuristic.
    if let Some(parent) = node.parent() {
        let mut pcur = parent.walk();
        let mut prev_comment: Option<Node<'_>> = None;
        for child in parent.children(&mut pcur) {
            if child.kind() == "comment" {
                prev_comment = Some(child);
            }
            if child.id() == node.id() {
                break;
            }
        }
        if let Some(c) = prev_comment {
            let text = node_text(c, src);
            if text.contains("@deprecated") {
                effects.push(v::EFFECT_DEPRECATED);
            }
        }
    }
    // Test heuristics: function name starts with `test`, or surrounding
    // call expression's callee is `describe`/`it`/`test` (jest/vitest).
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = node_text(name_node, src);
        if name.starts_with("test") {
            effects.push(v::EFFECT_TEST);
        }
    }
    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

/// Pull generics from a TS `type_parameters: (type_parameters …)` field.
pub(super) fn generics_for_function(
    node: Node<'_>,
    src: &str,
) -> Vec<crate::parsing::symbols::GenericParam> {
    let mut out: Vec<crate::parsing::symbols::GenericParam> = Vec::new();
    let Some(tparams) = node.child_by_field_name("type_parameters") else {
        return out;
    };
    let mut cursor = tparams.walk();
    for child in tparams.named_children(&mut cursor) {
        if child.kind() != "type_parameter" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, src).to_string();
        let mut bounds: Vec<String> = Vec::new();
        if let Some(constraint) = child.child_by_field_name("constraint") {
            let txt = node_text(constraint, src)
                .trim()
                .trim_start_matches("extends")
                .trim()
                .to_string();
            if !txt.is_empty() {
                bounds.push(txt);
            }
        }
        let default = child
            .child_by_field_name("value")
            .map(|n| node_text(n, src).to_string());
        out.push(crate::parsing::symbols::GenericParam {
            name,
            bounds,
            default,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_ts(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .expect("set_language ts");
        p.parse(src, None).expect("parse")
    }

    fn first_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if let Some(found) = first_of_kind(child, kind) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn parameters_with_types() {
        let src = "function f(x: number, y: string): boolean { return true; }";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name.as_deref(), Some("x"));
        assert!(parsed[0].type_tags.contains(&v::TAG_FLOAT.to_string()));
        assert!(parsed[1].type_tags.contains(&v::TAG_STRING.to_string()));
    }

    #[test]
    fn parameters_no_types_for_plain_js_style() {
        let src = "function plain(a, b) { return a + b; }";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].type_raw.is_none());
        assert!(parsed[0].type_tags.is_empty());
    }

    #[test]
    fn array_type_marks_container() {
        let src = "function f(xs: number[]): void {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(tags.contains(&v::TAG_FLOAT.to_string()));
    }

    #[test]
    fn union_with_undefined_marks_option() {
        let src = "function f(x: number | undefined): void {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_UNION.to_string()));
        assert!(tags.contains(&v::TAG_OPTION.to_string()));
        assert!(tags.contains(&v::TAG_NULL_LIKE.to_string()));
    }

    #[test]
    fn promise_marks_future() {
        let src = "async function f(): Promise<boolean> { return true; }";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let rt = return_type_from_node(fn_node.child_by_field_name("return_type"), src);
        assert!(rt.type_tags.contains(&v::TAG_FUTURE.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_ASYNC.to_string()));
    }

    #[test]
    fn async_function_has_async_effect() {
        let src = "async function f() {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_ASYNC.to_string()));
    }

    #[test]
    fn generator_function_has_generator_effect() {
        let src = "function* g() { yield 1; }";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "generator_function_declaration")
            .or_else(|| first_of_kind(tree.root_node(), "function_declaration"))
            .expect("fn");
        let effects = effects_for_function(fn_node, src);
        assert!(effects.contains(&v::EFFECT_GENERATOR.to_string()));
    }

    #[test]
    fn rest_parameter_is_variadic() {
        let src = "function f(...rest: number[]): void {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_variadic);
        assert_eq!(parsed[0].name.as_deref(), Some("rest"));
    }

    #[test]
    fn optional_parameter_marked_kwonly() {
        let src = "function f(x?: number): void {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].modifier, Some(ParamModifier::KwOnly));
    }

    #[test]
    fn generics_with_constraint() {
        let src = "function pick<T extends Foo>(a: T, b: T): T { return a; }";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let generics = generics_for_function(fn_node, src);
        assert_eq!(generics.len(), 1);
        assert_eq!(generics[0].name, "T");
        assert!(
            generics[0].bounds.iter().any(|b| b.contains("Foo")),
            "expected Foo bound, got {:?}",
            generics[0].bounds
        );
    }

    #[test]
    fn map_marks_keyed() {
        let src = "function f(m: Map<string, number>): void {}";
        let tree = parse_ts(src);
        let fn_node = first_of_kind(tree.root_node(), "function_declaration").expect("fn");
        let params = fn_node.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        let tags = &parsed[0].type_tags;
        assert!(tags.contains(&v::TAG_KEYED.to_string()));
        assert!(tags.contains(&v::TAG_CONTAINER.to_string()));
    }
}
