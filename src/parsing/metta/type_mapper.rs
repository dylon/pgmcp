//! Shadow-ASR extraction for MeTTa (atom-space / term-rewriting).
//!
//! MeTTa is unusual among pgmcp's backends: it carries explicit type
//! annotations via `(: name Type)`, so unlike Coq/TLA+/Lean, we can
//! surface real `type_raw` + `type_tags`. Rules (`(= LHS RHS)` and
//! `(:= name body)`) emit `term_rewrite` effects and structured
//! parameters drawn from the LHS pattern.
//!
//! Constructor lookups use the MeTTa-specific tag family (`atom`,
//! `expression`, `space`, `pattern_variable`, `metta_typed`,
//! `rule_head`, `rule_body`, `nondeterministic`) plus the universal
//! tags where they apply.

use tree_sitter::Node;

use crate::parsing::symbols::{ParamModifier, Parameter, ReturnType as SemReturnType};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Strip the `expression(...)` wrapper used by tree-sitter-metta to
/// hold semantic forms.
fn unwrap_expression(node: Node<'_>) -> Node<'_> {
    if node.kind() == "expression"
        && let Some(inner) = node.named_child(0)
    {
        return inner;
    }
    node
}

/// Strip the `atom_expression(...)` wrapper.
fn unwrap_atom(node: Node<'_>) -> Node<'_> {
    if node.kind() == "atom_expression"
        && let Some(inner) = node.named_child(0)
    {
        return inner;
    }
    node
}

/// Convert a MeTTa expression (type or value) into a `TypeShape`. Recursive.
pub(super) fn expr_to_shape(node: Node<'_>, src: &str) -> TypeShape {
    let raw = node_text(node, src).trim().to_string();
    let inner = unwrap_atom(unwrap_expression(node));
    let kind = inner.kind();
    match kind {
        "identifier" => TypeShape::leaf_raw(node_text(inner, src), raw),
        "variable" => TypeShape::leaf_raw(node_text(inner, src), raw),
        "string_literal" => TypeShape::leaf_raw("String", raw),
        "number_literal" | "integer_literal" | "decimal_literal" | "float_literal" => {
            TypeShape::leaf_raw("Number", raw)
        }
        "operator" => TypeShape::leaf_raw(node_text(inner, src), raw),
        "list" => {
            // First named child is the constructor (operator or identifier).
            // Remaining children are args.
            let mut cursor = inner.walk();
            let mut constructor = String::new();
            let mut args: Vec<TypeShape> = Vec::new();
            let mut first = true;
            for child in inner.named_children(&mut cursor) {
                let unwrapped = unwrap_atom(unwrap_expression(child));
                if first {
                    constructor = match unwrapped.kind() {
                        "identifier" => node_text(unwrapped, src).to_string(),
                        "operator" => node_text(unwrapped, src).to_string(),
                        _ => "List".to_string(),
                    };
                    first = false;
                } else {
                    args.push(expr_to_shape(child, src));
                }
            }
            if constructor.is_empty() {
                constructor = "List".to_string();
            }
            TypeShape::applied_raw(constructor, args, raw)
        }
        _ => TypeShape::leaf_raw(node_text(inner, src).trim(), raw),
    }
}

/// Build the type-tag set for a MeTTa type expression. The expression
/// must be drawn from a `(: name TypeExpr)` annotation or from a rule
/// body inferred-return-type position.
pub(super) fn type_tags_for_expr(node: Node<'_>, src: &str) -> Vec<String> {
    let mut tags: Vec<&'static str> = Vec::new();
    populate_tags(node, src, &mut tags);
    let mut owned: Vec<String> = tags.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

fn populate_tags(node: Node<'_>, src: &str, tags: &mut Vec<&'static str>) {
    let inner = unwrap_atom(unwrap_expression(node));
    let kind = inner.kind();
    match kind {
        "identifier" => {
            let name = node_text(inner, src);
            tag_constructor(name, tags);
            // Every MeTTa identifier participates in atom-space — also tag.
            tags.push(v::TAG_ATOM);
        }
        "variable" => {
            tags.push(v::TAG_PATTERN_VARIABLE);
            tags.push(v::TAG_ATOM);
        }
        "string_literal" => {
            tags.push(v::TAG_STRING);
            tags.push(v::TAG_ATOM);
        }
        "integer_literal" | "number_literal" => {
            tags.push(v::TAG_INT);
            tags.push(v::TAG_ATOM);
        }
        "decimal_literal" | "float_literal" => {
            tags.push(v::TAG_FLOAT);
            tags.push(v::TAG_ATOM);
        }
        "operator" => {
            // MeTTa operators (`:`, `=`, `:=`) are first-class atoms.
            tags.push(v::TAG_ATOM);
            tags.push(v::TAG_OPAQUE);
        }
        "list" => {
            tags.push(v::TAG_EXPRESSION);
            // Inspect the head to apply constructor-specific tags
            // (`->` function arrow, `List`, `Set`, `Map`, etc.).
            if let Some(head) = inner.named_child(0) {
                let head_inner = unwrap_atom(unwrap_expression(head));
                let head_text = node_text(head_inner, src);
                tag_constructor(head_text, tags);
            }
        }
        _ => tags.push(v::TAG_UNKNOWN),
    }
}

/// Tag a MeTTa constructor identifier. Primitive types and stdlib
/// constructors get the appropriate universal tags; MeTTa-specific
/// constructors get the MeTTa tag family.
fn tag_constructor(name: &str, tags: &mut Vec<&'static str>) {
    if tag_primitive(name, tags) {
        return;
    }
    match name {
        // MeTTa function-arrow type: `(-> T U R)` is a function type.
        "->" => {
            tags.push(v::TAG_FUNCTION);
        }
        // Standard MeTTa stdlib types.
        "List" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
            tags.push(v::TAG_INDEXED);
            tags.push(v::TAG_ORDERED);
        }
        "Set" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_UNORDERED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        "Map" | "Dict" => {
            tags.push(v::TAG_CONTAINER);
            tags.push(v::TAG_KEYED);
            tags.push(v::TAG_OWNED);
            tags.push(v::TAG_DYNAMIC);
        }
        // Pattern-class shorthand sometimes used.
        "Maybe" | "Optional" => {
            tags.push(v::TAG_OPTION);
            tags.push(v::TAG_NULL_LIKE);
        }
        "Either" | "Result" => {
            tags.push(v::TAG_RESULT);
            tags.push(v::TAG_SUM_TYPE);
        }
        // MeTTa atom-space type ("space").
        "Atom" => {
            tags.push(v::TAG_ATOM);
        }
        "Expression" => {
            tags.push(v::TAG_EXPRESSION);
        }
        "Space" | "GroundedSpace" => {
            tags.push(v::TAG_SPACE);
        }
        _ => {
            // Unknown constructor — left untagged; the raw text is the
            // user-facing identifier.
        }
    }
}

fn tag_primitive(name: &str, tags: &mut Vec<&'static str>) -> bool {
    match name {
        "Number" => {
            tags.push(v::TAG_FLOAT);
            true
        }
        "Integer" | "Int" => {
            tags.push(v::TAG_INT);
            true
        }
        "String" => {
            tags.push(v::TAG_STRING);
            true
        }
        "Bool" | "Boolean" => {
            tags.push(v::TAG_BOOL);
            true
        }
        "True" | "False" => {
            tags.push(v::TAG_BOOL);
            true
        }
        "Symbol" => {
            tags.push(v::TAG_ATOM);
            true
        }
        "Empty" | "Nil" => {
            tags.push(v::TAG_UNIT);
            true
        }
        _ => false,
    }
}

/// Pull parameters from a rule's LHS expression. For `(= (head $a $b) body)`,
/// returns `[$a, $b]` with `is_self=false`, modifier=Own, type info derived
/// from variable name and (if present) accompanying `(: head (-> A B R))`.
pub(super) fn parameters_from_rule_lhs(
    lhs_node: Node<'_>,
    src: &str,
    function_type: Option<&[TypeShape]>,
) -> Vec<Parameter> {
    let inner = unwrap_expression(lhs_node);
    if inner.kind() != "list" {
        return Vec::new();
    }
    let mut cursor = inner.walk();
    let mut out: Vec<Parameter> = Vec::new();
    let mut position: u32 = 0;
    let mut saw_head = false;
    for child in inner.named_children(&mut cursor) {
        let unwrapped = unwrap_atom(unwrap_expression(child));
        // Skip the head identifier — it's the rule's name, not a parameter.
        if !saw_head {
            match unwrapped.kind() {
                "identifier" | "operator" => {
                    saw_head = true;
                    continue;
                }
                _ => {
                    // No head — leave the LHS as-is. Don't emit parameters.
                    return Vec::new();
                }
            }
        }
        let kind = unwrapped.kind();
        // Function-type position [position] gives the parameter's type.
        let type_shape_from_sig = function_type.and_then(|ft| ft.get(position as usize).cloned());
        let mut type_tags: Vec<String> = Vec::new();
        if kind == "variable" {
            type_tags.push(v::TAG_PATTERN_VARIABLE.to_string());
            type_tags.push(v::TAG_ATOM.to_string());
        } else {
            type_tags.push(v::TAG_ATOM.to_string());
        }
        if let Some(shape) = &type_shape_from_sig {
            let mut more = Vec::new();
            tag_constructor(&shape.constructor, &mut more);
            for t in more {
                type_tags.push(t.to_string());
            }
        }
        type_tags.sort();
        type_tags.dedup();
        let name = match kind {
            "variable" => Some(node_text(unwrapped, src).to_string()),
            "identifier" => Some(node_text(unwrapped, src).to_string()),
            _ => None,
        };
        let type_raw = type_shape_from_sig.as_ref().and_then(|s| s.raw.clone());
        out.push(Parameter {
            position,
            name,
            type_raw,
            type_tags,
            type_shape: type_shape_from_sig,
            default_value: None,
            modifier: Some(ParamModifier::Own),
            is_variadic: false,
            is_self: false,
        });
        position += 1;
    }
    out
}

/// Decompose a function-type expression `(-> T1 T2 ... Tn R)` into its
/// per-position type-shape list. The last element is the return type;
/// preceding elements are parameter types. Returns `None` when the
/// expression isn't a function-arrow form.
pub(super) fn decompose_arrow_type(node: Node<'_>, src: &str) -> Option<Vec<TypeShape>> {
    let inner = unwrap_expression(node);
    if inner.kind() != "list" {
        return None;
    }
    let mut cursor = inner.walk();
    let mut head_ok = false;
    let mut parts: Vec<TypeShape> = Vec::new();
    for child in inner.named_children(&mut cursor) {
        let unwrapped = unwrap_atom(unwrap_expression(child));
        if !head_ok {
            let head_text = node_text(unwrapped, src).trim();
            if head_text != "->" {
                return None;
            }
            head_ok = true;
            continue;
        }
        parts.push(expr_to_shape(child, src));
    }
    Some(parts)
}

/// Build a `ReturnType` from a MeTTa type annotation's RHS expression.
/// When the RHS is a function-arrow `(-> A B R)`, the return type is `R`.
/// Otherwise the whole annotation is taken as the return type.
pub(super) fn return_type_from_annotation(rhs: Node<'_>, src: &str) -> SemReturnType {
    if let Some(parts) = decompose_arrow_type(rhs, src)
        && let Some(ret) = parts.last()
    {
        return SemReturnType {
            type_raw: ret.raw.clone(),
            type_tags: type_tags_for_constructor(&ret.constructor),
            type_shape: Some(ret.clone()),
        };
    }
    let shape = expr_to_shape(rhs, src);
    let type_raw = shape.raw.clone();
    SemReturnType {
        type_raw,
        type_tags: type_tags_for_expr(rhs, src),
        type_shape: Some(shape),
    }
}

/// Helper: just tag-constructor lookup for a `Vec<String>`.
fn type_tags_for_constructor(name: &str) -> Vec<String> {
    let mut tags: Vec<&'static str> = Vec::new();
    tag_constructor(name, &mut tags);
    let mut owned: Vec<String> = tags.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

/// Compute the effect set for a MeTTa rule given its operator (`=`, `:=`, `:`)
/// and LHS / RHS expressions.
pub(super) fn effects_for_rule(
    op_text: &str,
    lhs_node: Node<'_>,
    rhs_node: Node<'_>,
    src: &str,
) -> Vec<String> {
    let mut effects: Vec<&'static str> = Vec::new();
    match op_text {
        "=" => {
            effects.push(v::EFFECT_TERM_REWRITE);
            if lhs_contains_pattern(lhs_node) {
                effects.push(v::EFFECT_PATTERN_MATCH);
            }
        }
        ":=" => {
            effects.push(v::EFFECT_TERM_REWRITE);
        }
        ":" => {
            // Pure type annotation — no execution effect.
        }
        _ => {}
    }
    // Scan the body for top-level execution prefixes `!(...)` → metta_execute.
    if rhs_node_contains_execute_prefix(rhs_node, src) {
        effects.push(v::EFFECT_METTA_EXECUTE);
    }
    // Scan the body for `space_modify` / `import!` calls.
    if rhs_node_contains_call(rhs_node, src, "add-atom")
        || rhs_node_contains_call(rhs_node, src, "remove-atom")
    {
        effects.push(v::EFFECT_SPACE_MODIFY);
    }
    if rhs_node_contains_call(rhs_node, src, "import!") {
        effects.push(v::EFFECT_SPACE_IMPORT);
    }
    let mut owned: Vec<String> = effects.into_iter().map(String::from).collect();
    owned.sort();
    owned.dedup();
    owned
}

/// True when the LHS contains at least one non-variable atom in addition
/// to the head — i.e., a literal or compound pattern that triggers
/// matching beyond a bare-name rule.
fn lhs_contains_pattern(lhs_node: Node<'_>) -> bool {
    let inner = unwrap_expression(lhs_node);
    if inner.kind() != "list" {
        return false;
    }
    let mut cursor = inner.walk();
    let mut saw_head = false;
    for child in inner.named_children(&mut cursor) {
        let unwrapped = unwrap_atom(unwrap_expression(child));
        if !saw_head {
            saw_head = true;
            continue;
        }
        match unwrapped.kind() {
            "variable" => {}       // variable arg — bare binding, not a pattern
            "list" => return true, // nested expression → real pattern
            // tree-sitter-metta emits `integer_literal`, `float_literal`,
            // `string_literal` (and historically `number_literal`); accept
            // every literal kind here.
            "string_literal" | "number_literal" | "integer_literal" | "float_literal"
            | "decimal_literal" => return true,
            _ => {}
        }
    }
    false
}

/// True when the RHS subtree contains a `!(expr)` execution prefix.
fn rhs_node_contains_execute_prefix(node: Node<'_>, src: &str) -> bool {
    let kind = node.kind();
    if kind == "execute" || kind == "exec_expression" {
        return true;
    }
    // tree-sitter-metta may use a different name; conservative scan.
    let text = node_text(node, src);
    text.contains("!(")
}

fn rhs_node_contains_call(node: Node<'_>, src: &str, name: &str) -> bool {
    node_text(node, src).contains(name)
}

/// Detect if a MeTTa expression head is a variable (`$x`) — those forms
/// don't define a symbol, they bind.
#[allow(dead_code)]
pub(super) fn lhs_is_variable_only(lhs_node: Node<'_>, _src: &str) -> bool {
    let inner = unwrap_expression(lhs_node);
    inner.kind() == "atom_expression" && {
        let unwrapped = unwrap_atom(inner);
        unwrapped.kind() == "variable"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_metta(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_metta::language())
            .expect("set_language metta");
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
    fn tag_constructor_int_returns_int() {
        let mut tags: Vec<&'static str> = Vec::new();
        tag_constructor("Integer", &mut tags);
        assert!(tags.contains(&v::TAG_INT));
    }

    #[test]
    fn tag_constructor_arrow_marks_function() {
        let mut tags: Vec<&'static str> = Vec::new();
        tag_constructor("->", &mut tags);
        assert!(tags.contains(&v::TAG_FUNCTION));
    }

    #[test]
    fn tag_constructor_list_marks_container() {
        let mut tags: Vec<&'static str> = Vec::new();
        tag_constructor("List", &mut tags);
        assert!(tags.contains(&v::TAG_CONTAINER));
        assert!(tags.contains(&v::TAG_INDEXED));
    }

    #[test]
    fn tag_constructor_space_marks_space() {
        let mut tags: Vec<&'static str> = Vec::new();
        tag_constructor("GroundedSpace", &mut tags);
        assert!(tags.contains(&v::TAG_SPACE));
    }

    #[test]
    fn type_tags_for_int_annotation() {
        let src = "(: x Integer)";
        let tree = parse_metta(src);
        // Find the RHS (the Integer expression).
        let list = first_of_kind(tree.root_node(), "list").expect("list");
        // The third named child should be the type expression `Integer`.
        let mut named = Vec::new();
        let mut cursor = list.walk();
        for c in list.named_children(&mut cursor) {
            named.push(c);
        }
        let rhs = named.last().expect("rhs");
        let tags = type_tags_for_expr(*rhs, src);
        assert!(
            tags.contains(&v::TAG_INT.to_string()),
            "expected int tag, got {tags:?}"
        );
        assert!(tags.contains(&v::TAG_ATOM.to_string()));
    }

    #[test]
    fn type_tags_for_arrow_type_marks_function() {
        let src = "(: f (-> Integer Integer Bool))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let last = list.named_children(&mut cursor).last().expect("rhs expr");
        let tags = type_tags_for_expr(last, src);
        assert!(tags.contains(&v::TAG_FUNCTION.to_string()));
    }

    #[test]
    fn decompose_arrow_type_extracts_parts() {
        let src = "(: f (-> Integer String Bool))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let last = list.named_children(&mut cursor).last().expect("rhs expr");
        let parts = decompose_arrow_type(last, src).expect("arrow type");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].constructor, "Integer");
        assert_eq!(parts[1].constructor, "String");
        assert_eq!(parts[2].constructor, "Bool");
    }

    #[test]
    fn return_type_from_arrow_picks_last() {
        let src = "(: f (-> Integer Integer Bool))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let rhs = list.named_children(&mut cursor).last().expect("rhs expr");
        let rt = return_type_from_annotation(rhs, src);
        assert_eq!(rt.type_raw.as_deref(), Some("Bool"));
        assert!(rt.type_tags.contains(&v::TAG_BOOL.to_string()));
    }

    #[test]
    fn parameters_from_rule_lhs_strips_head_and_keeps_args() {
        let src = "(= (foo $a $b) (+ $a $b))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        // Second named child is the LHS list.
        let mut iter = list.named_children(&mut cursor);
        let _op = iter.next();
        let lhs = iter.next().expect("lhs");
        let params = parameters_from_rule_lhs(lhs, src, None);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name.as_deref(), Some("$a"));
        assert_eq!(params[1].name.as_deref(), Some("$b"));
        assert!(
            params[0]
                .type_tags
                .contains(&v::TAG_PATTERN_VARIABLE.to_string())
        );
    }

    #[test]
    fn parameters_use_function_type_when_present() {
        let src = "(= (foo $a $b) (+ $a $b))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let mut iter = list.named_children(&mut cursor);
        let _op = iter.next();
        let lhs = iter.next().expect("lhs");
        // Simulated function type from `(: foo (-> Integer Integer Integer))`.
        let function_type = vec![
            TypeShape::leaf_raw("Integer", "Integer"),
            TypeShape::leaf_raw("Integer", "Integer"),
            TypeShape::leaf_raw("Integer", "Integer"),
        ];
        let params = parameters_from_rule_lhs(lhs, src, Some(&function_type));
        assert_eq!(params.len(), 2);
        assert!(params[0].type_tags.contains(&v::TAG_INT.to_string()));
        assert_eq!(params[0].type_raw.as_deref(), Some("Integer"));
    }

    #[test]
    fn effects_for_eq_rule_marks_term_rewrite() {
        let src = "(= (foo $x) (+ $x 1))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let mut iter = list.named_children(&mut cursor);
        let _op = iter.next();
        let lhs = iter.next().expect("lhs");
        let rhs = iter.next().expect("rhs");
        let effects = effects_for_rule("=", lhs, rhs, src);
        assert!(effects.contains(&v::EFFECT_TERM_REWRITE.to_string()));
    }

    #[test]
    fn effects_for_colon_annotation_is_empty() {
        let src = "(: foo Integer)";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let mut iter = list.named_children(&mut cursor);
        let _op = iter.next();
        let lhs = iter.next().expect("lhs");
        let rhs = iter.next().expect("rhs");
        let effects = effects_for_rule(":", lhs, rhs, src);
        assert!(!effects.contains(&v::EFFECT_TERM_REWRITE.to_string()));
    }

    #[test]
    fn lhs_with_literal_marks_pattern_match() {
        let src = "(= (foo 5) (* 5 2))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let mut iter = list.named_children(&mut cursor);
        let _op = iter.next();
        let lhs = iter.next().expect("lhs");
        let rhs = iter.next().expect("rhs");
        let effects = effects_for_rule("=", lhs, rhs, src);
        assert!(effects.contains(&v::EFFECT_PATTERN_MATCH.to_string()));
    }

    #[test]
    fn expr_to_shape_nested_list() {
        let src = "(: foo (List Integer))";
        let tree = parse_metta(src);
        let list = first_of_kind(tree.root_node(), "list").expect("outer list");
        let mut cursor = list.walk();
        let rhs = list.named_children(&mut cursor).last().expect("rhs expr");
        let shape = expr_to_shape(rhs, src);
        assert_eq!(shape.constructor, "List");
        assert_eq!(shape.args.len(), 1);
        assert_eq!(shape.args[0].constructor, "Integer");
    }
}
