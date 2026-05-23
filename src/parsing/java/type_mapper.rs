//! Shadow-ASR extraction for Java.
//!
//! Java has formal parameters with type annotations, generics with bounds,
//! `throws` clauses, and annotation-based effect markers (`@Deprecated`,
//! `@Test`, `@Override`). The mapper extracts those into the shadow-ASR
//! fields. Method-overload disambiguation is left to downstream
//! resolution.

use tree_sitter::Node;

use crate::parsing::symbols::{ParamModifier, Parameter, ReturnType as SemReturnType};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

pub(super) fn type_to_shape(node: Node<'_>, src: &str) -> TypeShape {
    let raw = node_text(node, src).trim().to_string();
    let kind = node.kind();
    match kind {
        "integral_type"
        | "floating_point_type"
        | "boolean_type"
        | "void_type"
        | "type_identifier" => TypeShape::leaf_raw(node_text(node, src), raw),
        "generic_type" => {
            // First child is type_identifier (constructor), second is type_arguments.
            let constructor = node
                .named_child(0)
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            if let Some(targs) = node.child(1)
                && targs.kind() == "type_arguments"
            {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    args.push(type_to_shape(child, src));
                }
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        "array_type" => {
            let elem = node.named_child(0).map(|n| type_to_shape(n, src));
            TypeShape::applied_raw("Array", elem.map(|e| vec![e]).unwrap_or_default(), raw)
        }
        "scoped_type_identifier" => {
            // `java.util.List` etc. — keep the last segment as constructor.
            let last = node_text(node, src)
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            TypeShape::leaf_raw(last, raw)
        }
        _ => TypeShape::leaf_raw(node_text(node, src).trim(), raw),
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
    match kind {
        "integral_type" => tag_primitive_keyword(node_text(node, src), tags),
        "floating_point_type" => {
            tags.push(v::TAG_FLOAT);
        }
        "boolean_type" => tags.push(v::TAG_BOOL),
        "void_type" => tags.push(v::TAG_UNIT),
        "type_identifier" => tag_constructor(node_text(node, src), tags),
        "scoped_type_identifier" => {
            let last = node_text(node, src)
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            tag_constructor(&last, tags);
        }
        "generic_type" => {
            let constructor = node
                .named_child(0)
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&constructor, tags);
            // Recurse into type_arguments.
            if let Some(targs) = node.child(1)
                && targs.kind() == "type_arguments"
            {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    let mut inner = Vec::new();
                    populate_tags(child, src, &mut inner);
                    tags.extend(inner);
                }
            }
        }
        "array_type" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_FIXED_SIZE);
        }
        _ => {}
    }
}

fn tag_primitive_keyword(name: &str, tags: &mut Vec<&'static str>) {
    match name.trim() {
        "byte" | "short" | "int" | "long" | "char" => tags.push(v::TAG_INT),
        "float" | "double" => tags.push(v::TAG_FLOAT),
        _ => tags.push(v::TAG_INT),
    }
}

fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    match name {
        "Integer" | "Long" | "Short" | "Byte" | "BigInteger" => tags.push(v::TAG_INT),
        "Double" | "Float" | "BigDecimal" => tags.push(v::TAG_FLOAT),
        "String" | "CharSequence" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
        }
        "Boolean" => tags.push(v::TAG_BOOL),
        "Character" => tags.push(v::TAG_CHAR),
        "List" | "ArrayList" | "LinkedList" | "Vector" | "Stack" | "Deque" | "ArrayDeque" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
        }
        "Map" | "HashMap" | "ConcurrentHashMap" | "LinkedHashMap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "LinkedHashMap" {
                tags.push(v::TAG_ORDERED);
            } else {
                tags.push(v::TAG_UNORDERED);
            }
        }
        "TreeMap" | "SortedMap" | "NavigableMap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_ORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        "Set" | "HashSet" | "LinkedHashSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        "TreeSet" | "SortedSet" | "NavigableSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_ORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        "Optional" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
        }
        "Future" | "CompletableFuture" | "CompletionStage" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
        }
        "Stream" | "IntStream" | "LongStream" | "DoubleStream" => {
            tags.push(v::TAG_STREAM);
            tags.push(v::TAG_ITERATOR);
        }
        "Iterable" | "Iterator" => tags.push(v::TAG_ITERATOR),
        "Function" | "BiFunction" | "Supplier" | "Consumer" | "Predicate" | "Runnable"
        | "Callable" => tags.push(v::TAG_FUNCTION),
        "Path" | "File" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_FILESYSTEM);
        }
        "Exception" | "RuntimeException" | "Error" | "Throwable" => {
            tags.push(v::TAG_ERROR_TYPE);
        }
        "Lock" | "ReentrantLock" | "ReadWriteLock" => {
            tags.push(v::TAG_MUTEX);
            tags.push(v::TAG_CONCURRENCY);
        }
        "AtomicInteger" | "AtomicLong" | "AtomicReference" | "AtomicBoolean" => {
            tags.push(v::TAG_ATOMIC);
            tags.push(v::TAG_CONCURRENCY);
        }
        _ => {}
    }
}

pub(super) fn parameters_from_node(formals: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut cursor = formals.walk();
    for (position, child) in (0_u32..).zip(formals.named_children(&mut cursor)) {
        let kind = child.kind();
        if !matches!(kind, "formal_parameter" | "spread_parameter") {
            continue;
        }
        let is_variadic = kind == "spread_parameter";
        let type_node = child.child_by_field_name("type");
        let name_node = child.child_by_field_name("name");
        let name = name_node.map(|n| node_text(n, src).to_string());
        let (type_raw, type_tags, type_shape) = match type_node {
            Some(t) => (
                Some(node_text(t, src).trim().to_string()),
                type_tags_for(t, src),
                Some(type_to_shape(t, src)),
            ),
            None => (None, Vec::new(), None),
        };
        out.push(Parameter {
            position,
            name,
            type_raw,
            type_tags,
            type_shape,
            default_value: None,
            modifier: Some(ParamModifier::Own),
            is_variadic,
            is_self: false,
        });
    }
    out
}

pub(super) fn return_type_from_method(method: Node<'_>, src: &str) -> SemReturnType {
    let Some(type_node) = method.child_by_field_name("type") else {
        return SemReturnType {
            type_raw: None,
            type_tags: Vec::new(),
            type_shape: None,
        };
    };
    SemReturnType {
        type_raw: Some(node_text(type_node, src).trim().to_string()),
        type_tags: type_tags_for(type_node, src),
        type_shape: Some(type_to_shape(type_node, src)),
    }
}

pub(super) fn effects_for_method(method: Node<'_>, src: &str) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    // Walk modifiers + annotations for effect markers.
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mod_cursor = child.walk();
        for m in child.named_children(&mut mod_cursor) {
            let mk = m.kind();
            match mk {
                "marker_annotation" => {
                    let name = m
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src))
                        .unwrap_or("");
                    match name {
                        "Deprecated" => effects.push(v::EFFECT_DEPRECATED),
                        "Override" => effects.push(v::EFFECT_OVERRIDE),
                        "Test" => effects.push(v::EFFECT_TEST),
                        _ => {}
                    }
                }
                "annotation" => {
                    let name = m
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src))
                        .unwrap_or("");
                    match name {
                        "Deprecated" => effects.push(v::EFFECT_DEPRECATED),
                        "Override" => effects.push(v::EFFECT_OVERRIDE),
                        "Test" | "ParameterizedTest" | "RepeatedTest" => {
                            effects.push(v::EFFECT_TEST)
                        }
                        _ => {}
                    }
                }
                "synchronized" => {
                    effects.push(v::EFFECT_BLOCKING_IO);
                    effects.push(v::EFFECT_UNSAFE);
                }
                "abstract" => effects.push(v::EFFECT_VIRTUAL),
                _ => {}
            }
        }
    }
    // `throws X, Y` clause → throws effect.
    let mut tcursor = method.walk();
    for child in method.named_children(&mut tcursor) {
        if child.kind() == "throws" {
            effects.push(v::EFFECT_THROWS);
            break;
        }
    }
    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

pub(super) fn generics_for_method(
    method: Node<'_>,
    src: &str,
) -> Vec<crate::parsing::symbols::GenericParam> {
    let mut out: Vec<crate::parsing::symbols::GenericParam> = Vec::new();
    let Some(tparams) = method.child_by_field_name("type_parameters") else {
        return out;
    };
    let mut cursor = tparams.walk();
    for child in tparams.named_children(&mut cursor) {
        if child.kind() != "type_parameter" {
            continue;
        }
        // First child is the type_identifier; subsequent are type_bound.
        let mut iter_cursor = child.walk();
        let mut iter = child.named_children(&mut iter_cursor);
        let name = iter
            .next()
            .map(|n| node_text(n, src).to_string())
            .unwrap_or_default();
        let mut bounds: Vec<String> = Vec::new();
        for bound in iter {
            if bound.kind() == "type_bound" {
                let mut bc = bound.walk();
                for bb in bound.named_children(&mut bc) {
                    bounds.push(node_text(bb, src).to_string());
                }
            }
        }
        out.push(crate::parsing::symbols::GenericParam {
            name,
            bounds,
            default: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_java(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("set_language java");
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
    fn parameters_with_int_and_string() {
        let src = "class Foo { void m(int x, String s) {} }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let params = method.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].type_tags.contains(&v::TAG_INT.to_string()));
        assert!(parsed[1].type_tags.contains(&v::TAG_STRING.to_string()));
    }

    #[test]
    fn list_marks_container_and_ordered() {
        let src = "class Foo { List<String> names() { return null; } }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let rt = return_type_from_method(method, src);
        assert!(rt.type_tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_STRING.to_string()));
    }

    #[test]
    fn optional_marks_option() {
        let src = "class Foo { Optional<Integer> find() { return null; } }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let rt = return_type_from_method(method, src);
        assert!(rt.type_tags.contains(&v::TAG_OPTION.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_INT.to_string()));
    }

    #[test]
    fn completable_future_marks_future() {
        let src = "class Foo { CompletableFuture<String> fetch() { return null; } }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let rt = return_type_from_method(method, src);
        assert!(rt.type_tags.contains(&v::TAG_FUTURE.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_ASYNC.to_string()));
    }

    #[test]
    fn throws_clause_emits_throws_effect() {
        let src = "class Foo { void m() throws IOException {} }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let effects = effects_for_method(method, src);
        assert!(effects.contains(&v::EFFECT_THROWS.to_string()));
    }

    #[test]
    fn deprecated_annotation_emits_effect() {
        let src = "class Foo { @Deprecated void m() {} }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let effects = effects_for_method(method, src);
        assert!(effects.contains(&v::EFFECT_DEPRECATED.to_string()));
    }

    #[test]
    fn override_annotation_emits_effect() {
        let src = "class Foo { @Override public String toString() { return \"\"; } }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let effects = effects_for_method(method, src);
        assert!(effects.contains(&v::EFFECT_OVERRIDE.to_string()));
    }

    #[test]
    fn generics_with_bounds() {
        let src = "class Foo { <T extends Comparable> T pick(T a, T b) { return a; } }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let gens = generics_for_method(method, src);
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].name, "T");
        assert!(gens[0].bounds.iter().any(|b| b.contains("Comparable")));
    }

    #[test]
    fn spread_parameter_is_variadic() {
        let src = "class Foo { void m(String... args) {} }";
        let tree = parse_java(src);
        let method = first_of_kind(tree.root_node(), "method_declaration").expect("method");
        let params = method.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_variadic);
    }
}
