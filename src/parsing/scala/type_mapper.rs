//! Shadow-ASR extraction for Scala.
//!
//! Scala AST surface roughly mirrors Java's: `function_definition` /
//! `class_definition` with `parameters: (parameters (parameter name: type:))`,
//! `return_type:`, optional `type_parameters: (type_parameters name: bound:)`,
//! and `(annotation name:)` decorators. Scala's standard library is
//! immutable-first (`List`, `Option`, `Future`, `Try`, etc.), which the
//! constructor table reflects.

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
        "type_identifier" => TypeShape::leaf_raw(node_text(node, src), raw),
        "generic_type" => {
            let constructor = node
                .child_by_field_name("type")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args: Vec<TypeShape> = Vec::new();
            if let Some(targs) = node.child_by_field_name("type_arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    args.push(type_to_shape(child, src));
                }
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        "tuple_type" => {
            let mut cursor = node.walk();
            let mut args = Vec::new();
            for child in node.named_children(&mut cursor) {
                args.push(type_to_shape(child, src));
            }
            TypeShape::applied_raw("Tuple", args, raw)
        }
        "function_type" => TypeShape::leaf_raw("Function", raw),
        "structural_type" => TypeShape::leaf_raw("Structural", raw),
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
        "type_identifier" => tag_constructor(node_text(node, src), tags),
        "generic_type" => {
            let constructor = node
                .child_by_field_name("type")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            tag_constructor(&constructor, tags);
            if let Some(targs) = node.child_by_field_name("type_arguments") {
                let mut cursor = targs.walk();
                for child in targs.named_children(&mut cursor) {
                    let mut inner = Vec::new();
                    populate_tags(child, src, &mut inner);
                    tags.extend(inner);
                }
            }
        }
        "tuple_type" => {
            tags.push(v::TAG_PRODUCT_TYPE);
            tags.push(v::TAG_FIXED_SIZE);
        }
        "function_type" => tags.push(v::TAG_FUNCTION),
        _ => {}
    }
}

fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    match name {
        "Int" | "Long" | "Short" | "Byte" | "BigInt" => tags.push(v::TAG_INT),
        "Double" | "Float" | "BigDecimal" => tags.push(v::TAG_FLOAT),
        "String" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_OWNED);
        }
        "Boolean" => tags.push(v::TAG_BOOL),
        "Char" => tags.push(v::TAG_CHAR),
        "Unit" => tags.push(v::TAG_UNIT),
        "Nothing" => tags.push(v::TAG_NEVER),
        "Any" | "AnyRef" | "AnyVal" => tags.push(v::TAG_UNKNOWN),
        "Null" => tags.push(v::TAG_NULL_LIKE),
        "List" | "Seq" | "Vector" | "IndexedSeq" | "ArraySeq" | "Array" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
            if name == "Array" {
                tags.push(v::TAG_FIXED_SIZE);
            } else {
                tags.push(v::TAG_DYNAMIC);
            }
        }
        "Map" | "HashMap" | "TreeMap" | "ListMap" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "TreeMap" {
                tags.push(v::TAG_ORDERED);
            } else {
                tags.push(v::TAG_UNORDERED);
            }
        }
        "Set" | "HashSet" | "TreeSet" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            if name == "TreeSet" {
                tags.push(v::TAG_ORDERED);
            } else {
                tags.push(v::TAG_UNORDERED);
            }
        }
        "Option" | "Some" | "None" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
        }
        "Either" | "Left" | "Right" | "Try" | "Success" | "Failure" => {
            tags.push(v::TAG_RESULT);
            tags.push(v::TAG_SUM_TYPE);
        }
        "Future" | "Promise" | "Task" | "IO" => {
            tags.push(v::TAG_FUTURE);
            tags.push(v::TAG_ASYNC);
        }
        "Iterator" | "Iterable" | "LazyList" | "Stream" => {
            tags.push(v::TAG_ITERATOR);
            if name == "LazyList" || name == "Stream" {
                tags.push(v::TAG_STREAM);
            }
        }
        "Function" | "Function0" | "Function1" | "Function2" | "Function3" | "Function4"
        | "Function5" => tags.push(v::TAG_FUNCTION),
        "Throwable" | "Exception" | "RuntimeException" | "Error" => tags.push(v::TAG_ERROR_TYPE),
        _ => {}
    }
}

pub(super) fn parameters_from_node(params_node: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut cursor = params_node.walk();
    for (position, child) in (0_u32..).zip(params_node.named_children(&mut cursor)) {
        if child.kind() != "parameter" {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .map(|n| node_text(n, src).to_string());
        let type_node = child.child_by_field_name("type");
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
            is_variadic: false,
            is_self: false,
        });
    }
    out
}

pub(super) fn return_type_from_function(node: Node<'_>, src: &str) -> SemReturnType {
    let Some(rt) = node.child_by_field_name("return_type") else {
        return SemReturnType {
            type_raw: None,
            type_tags: Vec::new(),
            type_shape: None,
        };
    };
    SemReturnType {
        type_raw: Some(node_text(rt, src).trim().to_string()),
        type_tags: type_tags_for(rt, src),
        type_shape: Some(type_to_shape(rt, src)),
    }
}

pub(super) fn effects_for_function(node: Node<'_>, src: &str) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "annotation"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, src);
            match name {
                "deprecated" | "Deprecated" => effects.push(v::EFFECT_DEPRECATED),
                "throws" => effects.push(v::EFFECT_THROWS),
                "inline" => effects.push(v::EFFECT_INLINE),
                "tailrec" => effects.push(v::EFFECT_PURE),
                "Test" | "test" => effects.push(v::EFFECT_TEST),
                _ => {}
            }
        }
        if child.kind() == "modifiers" {
            let mut mcursor = child.walk();
            for m in child.children(&mut mcursor) {
                match m.kind() {
                    "override_modifier" => effects.push(v::EFFECT_OVERRIDE),
                    "abstract_modifier" => effects.push(v::EFFECT_VIRTUAL),
                    _ => {}
                }
            }
        }
    }
    // Method name starting with "test" inside a class is a test heuristic.
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

pub(super) fn generics_for_function(node: Node<'_>, src: &str) -> Vec<GenericParam> {
    let mut out: Vec<GenericParam> = Vec::new();
    // Scala's tree-sitter emits multiple `parameters:` fields when the
    // function has type parameters. The first parameters field is
    // type_parameters; subsequent are value parameters.
    let mut cursor = node.walk();
    let mut tp_node: Option<Node<'_>> = None;
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some("parameters")
                && cursor.node().kind() == "type_parameters"
            {
                tp_node = Some(cursor.node());
                break;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    let Some(tparams) = tp_node else {
        return out;
    };
    let name = tparams
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return out;
    }
    let mut bounds: Vec<String> = Vec::new();
    if let Some(ub) = tparams.child_by_field_name("bound")
        && let Some(ty) = ub.child_by_field_name("type")
    {
        bounds.push(node_text(ty, src).to_string());
    }
    out.push(GenericParam {
        name,
        bounds,
        default: None,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_scala(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_scala::LANGUAGE.into())
            .expect("set_language scala");
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
    fn parameters_with_primitive_types() {
        let src = "def add(x: Int, y: Int): Int = x + y";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let params = func.child_by_field_name("parameters").expect("params");
        let parsed = parameters_from_node(params, src);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].type_tags.contains(&v::TAG_INT.to_string()));
    }

    #[test]
    fn option_marks_option_tag() {
        let src = "def find(x: Int): Option[String] = None";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_OPTION.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_STRING.to_string()));
    }

    #[test]
    fn future_marks_future_tag() {
        let src = "def fetch(): Future[String] = ???";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_FUTURE.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_ASYNC.to_string()));
    }

    #[test]
    fn list_marks_container_indexed_ordered() {
        let src = "def names(): List[String] = Nil";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_CONTAINER.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_INDEXED.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_ORDERED.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_STRING.to_string()));
    }

    #[test]
    fn map_marks_keyed() {
        let src = "def lookup(): Map[String, Int] = Map.empty";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_KEYED.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_CONTAINER.to_string()));
    }

    #[test]
    fn either_marks_result() {
        let src = "def either(): Either[String, Int] = ???";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_RESULT.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_SUM_TYPE.to_string()));
    }

    #[test]
    fn unit_marks_unit() {
        let src = "def f(): Unit = ()";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let rt = return_type_from_function(func, src);
        assert!(rt.type_tags.contains(&v::TAG_UNIT.to_string()));
    }

    #[test]
    fn deprecated_annotation_emits_effect() {
        let src = "@deprecated def old(): Unit = ()";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let effects = effects_for_function(func, src);
        assert!(effects.contains(&v::EFFECT_DEPRECATED.to_string()));
    }

    #[test]
    fn test_method_name_emits_test_effect() {
        let src = "def testSomething(): Unit = ()";
        let tree = parse_scala(src);
        let func = first_of_kind(tree.root_node(), "function_definition").expect("func");
        let effects = effects_for_function(func, src);
        assert!(effects.contains(&v::EFFECT_TEST.to_string()));
    }
}
