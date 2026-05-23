//! Shadow-ASR extraction for C and C++.
//!
//! C++ source surfaces `function_definition` with `type:` (return type) and
//! `declarator: (function_declarator parameters: (parameter_list ...))`.
//! Each `parameter_declaration` has a `type:` and `declarator:`. Reference
//! and pointer wrappers (`reference_declarator`, `pointer_declarator`)
//! signal pass-by-reference / pointer semantics. `type_qualifier` children
//! carry `const` / `volatile` flags.

use tree_sitter::Node;

use crate::parsing::symbols::{
    GenericParam, ParamModifier, Parameter, ReturnType as SemReturnType,
};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

pub(super) fn type_to_shape(node: Node<'_>, src: &str) -> TypeShape {
    let raw = node_text(node, src).trim().to_string();
    let kind = node.kind();
    match kind {
        "primitive_type" | "type_identifier" | "sized_type_specifier" => {
            TypeShape::leaf_raw(node_text(node, src).trim(), raw)
        }
        "template_type" => {
            let constructor = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            if let Some(targs) = node.child_by_field_name("arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    // child is `type_descriptor` wrapping a type.
                    if let Some(inner) = child.child_by_field_name("type") {
                        args.push(type_to_shape(inner, src));
                    } else {
                        args.push(type_to_shape(child, src));
                    }
                }
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        "qualified_identifier" => {
            // `std::vector` etc. — keep the inner name as constructor.
            let name_node = node.child_by_field_name("name").unwrap_or(node);
            type_to_shape(name_node, src)
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
    match node.kind() {
        "primitive_type" | "sized_type_specifier" => {
            tag_primitive(node_text(node, src), tags);
        }
        "type_identifier" => tag_constructor(node_text(node, src), tags),
        "template_type" => {
            let constructor = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&constructor, tags);
            if let Some(targs) = node.child_by_field_name("arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    if let Some(inner) = child.child_by_field_name("type") {
                        let mut inner_tags = Vec::new();
                        populate_tags(inner, src, &mut inner_tags);
                        tags.extend(inner_tags);
                    }
                }
            }
        }
        "qualified_identifier" => {
            // Strip the namespace and inspect the inner name.
            if let Some(name_node) = node.child_by_field_name("name") {
                let mut inner_tags = Vec::new();
                populate_tags(name_node, src, &mut inner_tags);
                tags.extend(inner_tags);
            }
        }
        _ => {}
    }
}

fn tag_primitive(name: &str, tags: &mut Vec<&'static str>) {
    let trimmed = name.trim();
    match trimmed {
        "void" => tags.push(v::TAG_UNIT),
        "bool" | "_Bool" => tags.push(v::TAG_BOOL),
        "char" | "char8_t" | "char16_t" | "char32_t" | "wchar_t" => tags.push(v::TAG_CHAR),
        "float" | "double" | "long double" => tags.push(v::TAG_FLOAT),
        _ => {
            // Most other primitives are integer types in C/C++.
            tags.push(v::TAG_INT);
        }
    }
}

fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    match name {
        "string" | "wstring" | "u8string" | "u16string" | "u32string" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
        }
        "string_view" | "wstring_view" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_BORROWED);
        }
        "vector" | "deque" | "list" | "forward_list" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
        }
        "array" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_FIXED_SIZE);
            tags.push(v::TAG_INDEXED);
        }
        "map" | "unordered_map" | "multimap" | "unordered_multimap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name.starts_with("unordered") {
                tags.push(v::TAG_UNORDERED);
            } else {
                tags.push(v::TAG_ORDERED);
            }
        }
        "set" | "unordered_set" | "multiset" | "unordered_multiset" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name.starts_with("unordered") {
                tags.push(v::TAG_UNORDERED);
            } else {
                tags.push(v::TAG_ORDERED);
            }
        }
        "optional" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
        }
        "expected" | "variant" => {
            tags.push(v::TAG_SUM_TYPE);
        }
        "tuple" | "pair" => tags.push(v::TAG_PRODUCT_TYPE),
        "unique_ptr" => {
            tags.push(v::TAG_SMART_POINTER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_UNIQUE);
        }
        "shared_ptr" => {
            tags.push(v::TAG_SMART_POINTER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_SHARED);
        }
        "weak_ptr" => tags.push(v::TAG_WEAK),
        "function" => tags.push(v::TAG_FUNCTION),
        "future" | "shared_future" | "promise" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
        }
        "mutex" | "recursive_mutex" | "shared_mutex" | "timed_mutex" | "recursive_timed_mutex" => {
            tags.push(v::TAG_MUTEX);
            tags.push(v::TAG_CONCURRENCY);
        }
        "atomic" | "atomic_flag" | "atomic_bool" | "atomic_int" | "atomic_long" | "atomic_uint"
        | "atomic_ulong" | "atomic_size_t" => {
            tags.push(v::TAG_ATOMIC);
            tags.push(v::TAG_CONCURRENCY);
        }
        "exception" | "runtime_error" | "logic_error" | "out_of_range" | "invalid_argument" => {
            tags.push(v::TAG_ERROR_TYPE)
        }
        "path" | "filesystem" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_FILESYSTEM);
        }
        _ => {}
    }
}

pub(super) fn parameters_from_node(params_node: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut cursor = params_node.walk();
    for (position, child) in (0_u32..).zip(params_node.named_children(&mut cursor)) {
        if child.kind() != "parameter_declaration" {
            continue;
        }
        let type_node = child.child_by_field_name("type");
        let decl_node = child.child_by_field_name("declarator");
        let (name, modifier, is_pointer, is_reference, is_mutable_ref) = match decl_node {
            Some(d) => extract_declarator(d, src),
            None => (None, Some(ParamModifier::Own), false, false, false),
        };
        let mut type_tags = match type_node {
            Some(t) => type_tags_for(t, src),
            None => Vec::new(),
        };
        if is_pointer {
            type_tags.push(v::TAG_POINTER.to_string());
        }
        if is_reference {
            type_tags.push(v::TAG_REFERENCE.to_string());
        }
        if is_mutable_ref {
            type_tags.push(v::TAG_MUTABLE_REF.to_string());
        }
        // const qualifier signals borrow. Detect via the raw parameter
        // text since tree-sitter-cpp emits `const` as a token inside a
        // `type_qualifier` node (not always as a named child).
        let param_text = node_text(child, src);
        let is_const = param_text.contains("const ")
            || param_text.starts_with("const")
            || has_const_qualifier(child);
        if is_const {
            type_tags.push(v::TAG_BORROWED.to_string());
        }
        type_tags.sort();
        type_tags.dedup();
        let type_raw = type_node.map(|t| node_text(t, src).trim().to_string());
        let type_shape = type_node.map(|t| type_to_shape(t, src));
        out.push(Parameter {
            position,
            name,
            type_raw,
            type_tags,
            type_shape,
            default_value: None,
            modifier,
            is_variadic: false,
            is_self: false,
        });
    }
    out
}

fn extract_declarator(
    node: Node<'_>,
    src: &str,
) -> (Option<String>, Option<ParamModifier>, bool, bool, bool) {
    match node.kind() {
        "identifier" => (
            Some(node_text(node, src).to_string()),
            Some(ParamModifier::Own),
            false,
            false,
            false,
        ),
        "pointer_declarator" => {
            let inner = node.child_by_field_name("declarator").unwrap_or(node);
            let (name, _, _, _, _) = extract_declarator(inner, src);
            (name, Some(ParamModifier::Own), true, false, false)
        }
        "reference_declarator" => {
            // r-value vs l-value: tree-sitter-cpp's `reference_declarator` covers both.
            // Default to immutable reference; const_qualifier on the type drives
            // is_mutable_ref off (caller handles `const T&` semantics).
            let inner = node.named_child(0).unwrap_or(node);
            let (name, _, _, _, _) = extract_declarator(inner, src);
            (name, Some(ParamModifier::Ref), false, true, true)
        }
        "array_declarator" => {
            let inner = node.child_by_field_name("declarator").unwrap_or(node);
            let (name, _, _, _, _) = extract_declarator(inner, src);
            (name, Some(ParamModifier::Own), false, false, false)
        }
        _ => (
            Some(node_text(node, src).to_string()),
            Some(ParamModifier::Own),
            false,
            false,
            false,
        ),
    }
}

fn has_const_qualifier(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_qualifier" {
            // tree-sitter-cpp emits `const` either as a named child or as an
            // anonymous token. Walk both options.
            let mut inner = child.walk();
            for c in child.children(&mut inner) {
                if c.kind() == "const" {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) fn return_type_from_function(node: Node<'_>, src: &str) -> SemReturnType {
    let Some(type_node) = node.child_by_field_name("type") else {
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

pub(super) fn effects_for_function(node: Node<'_>, src: &str) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    // `noexcept` shows up as a sibling of `parameters` inside the
    // function_declarator. Scan recursively for simplicity.
    if function_contains_kind(node, "noexcept") {
        effects.push(v::EFFECT_PURE);
    }
    // `[[deprecated]]` attribute.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_declaration"
            && let Some(attr) = child.named_child(0)
            && let Some(name_node) = attr.child_by_field_name("name")
        {
            let name = node_text(name_node, src);
            if name == "deprecated" {
                effects.push(v::EFFECT_DEPRECATED);
            }
            if name == "noreturn" {
                effects.push(v::EFFECT_MAY_PANIC);
            }
        }
    }
    // Heuristic: test functions named with a `test`/`Test` prefix.
    if let Some(decl) = node.child_by_field_name("declarator")
        && let Some(fname) = function_name_from_declarator(decl, src)
        && (fname.starts_with("test") || fname.starts_with("Test"))
    {
        effects.push(v::EFFECT_TEST);
    }
    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

fn function_contains_kind(node: Node<'_>, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if function_contains_kind(child, kind) {
            return true;
        }
    }
    false
}

fn function_name_from_declarator(node: Node<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, src).to_string()),
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| function_name_from_declarator(d, src)),
        _ => node
            .child_by_field_name("declarator")
            .and_then(|d| function_name_from_declarator(d, src))
            .or_else(|| {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier")
                    .map(|n| node_text(n, src).to_string())
            }),
    }
}

pub(super) fn generics_for_function(node: Node<'_>, src: &str) -> Vec<GenericParam> {
    // C++ templates wrap the function_definition with a template_declaration
    // having parameters: (template_parameter_list ...).
    let mut out: Vec<GenericParam> = Vec::new();
    let Some(parent) = node.parent() else {
        return out;
    };
    if parent.kind() != "template_declaration" {
        return out;
    }
    let Some(tparams) = parent.child_by_field_name("parameters") else {
        return out;
    };
    let mut cursor = tparams.walk();
    for child in tparams.named_children(&mut cursor) {
        if child.kind() == "type_parameter_declaration"
            && let Some(name_node) = child.named_child(0)
        {
            out.push(GenericParam {
                name: node_text(name_node, src).to_string(),
                bounds: Vec::new(),
                default: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_cpp(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("set_language cpp");
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

    fn first_function(tree: &tree_sitter::Tree) -> Option<Node<'_>> {
        first_of_kind(tree.root_node(), "function_definition")
    }

    #[test]
    fn parameters_with_int() {
        let src = "int add(int x, int y) { return 0; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let decl = func.child_by_field_name("declarator").expect("decl");
        let params = decl.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].type_tags.contains(&v::TAG_INT.to_string()));
    }

    #[test]
    fn return_type_primitive() {
        let src = "int add(int x) { return 0; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_INT.to_string()));
    }

    #[test]
    fn vector_marks_container() {
        let src = "std::vector<int> nums() { return {}; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_INDEXED.to_string()));
    }

    #[test]
    fn unique_ptr_marks_smart_pointer_unique() {
        let src = "std::unique_ptr<int> make() { return nullptr; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_SMART_POINTER.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_UNIQUE.to_string()));
    }

    #[test]
    fn shared_ptr_marks_shared() {
        let src = "std::shared_ptr<int> make() { return nullptr; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_SHARED.to_string()));
    }

    #[test]
    fn optional_marks_option() {
        let src = "std::optional<int> find() { return std::nullopt; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_OPTION.to_string()));
    }

    #[test]
    fn future_marks_future() {
        let src = "std::future<int> compute() { return {}; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_FUTURE.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_ASYNC.to_string()));
    }

    #[test]
    fn mutex_marks_concurrency() {
        let src = "std::mutex create() { return {}; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_MUTEX.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_CONCURRENCY.to_string()));
    }

    #[test]
    fn const_ref_parameter_marks_borrowed() {
        let src = "void f(const std::string& s) {}";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let decl = func.child_by_field_name("declarator").expect("decl");
        let params = decl.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].type_tags.contains(&v::TAG_REFERENCE.to_string()));
        assert!(parsed[0].type_tags.contains(&v::TAG_BORROWED.to_string()));
    }

    #[test]
    fn deprecated_attribute_emits_effect() {
        let src = "[[deprecated]] void old() {}";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let effects = effects_for_function(func, src);
        assert!(effects.contains(&v::EFFECT_DEPRECATED.to_string()));
    }

    #[test]
    fn noexcept_emits_pure_effect() {
        let src = "void f() noexcept {}";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let effects = effects_for_function(func, src);
        assert!(effects.contains(&v::EFFECT_PURE.to_string()));
    }

    #[test]
    fn template_function_extracts_generics() {
        let src = "template<typename T> T pick(T a, T b) { return a; }";
        let tree = parse_cpp(src);
        let func = first_function(&tree).expect("func");
        let gens = generics_for_function(func, src);
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].name, "T");
    }
}
