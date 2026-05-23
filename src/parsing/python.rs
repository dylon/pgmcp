//! Python language backend (Tier 0e). Uses `tree-sitter-python` for symbols,
//! imports, and references.
//!
//! `tree-sitter::Parser` is `!Sync`, so we keep one per thread via
//! `thread_local!`. `Query` is `Send + Sync`, so we cache the three queries
//! once per process via `OnceLock`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "python/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::complexity;
use crate::parsing::function_metrics::{
    CognitiveIncrement, CognitiveKind, FunctionMetrics, ScoringInput,
};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static PYTHON_BACKEND: PythonBackend = PythonBackend;
pub struct PythonBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("set_language python");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(class_definition name: (identifier) @class.name) @class.def
(function_definition name: (identifier) @fn.name) @fn.def
(module
  (expression_statement
    (assignment
      left: (identifier) @const.name)))
"#;

const IMPORT_QUERY: &str = r#"
(import_statement) @import.stmt
(import_from_statement) @from.stmt
"#;

const REF_QUERY: &str = r#"
(call function: (identifier) @call.name) @call.expr
(call function: (attribute attribute: (identifier) @call.method)) @mcall.expr
(class_definition
  superclasses: (argument_list (identifier) @inherit.name))
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_python::LANGUAGE.into(), SYMBOL_QUERY).expect("symbol query python")
    })
}

fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_python::LANGUAGE.into(), IMPORT_QUERY).expect("import query python")
    })
}

fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_python::LANGUAGE.into(), REF_QUERY).expect("ref query python")
    })
}

fn parse(content: &str) -> Option<Tree> {
    PARSER.with(|p| p.borrow_mut().parse(content, None))
}

fn line_of(node: Node<'_>) -> u32 {
    (node.start_position().row as u32) + 1
}

fn end_line_of(node: Node<'_>) -> u32 {
    (node.end_position().row as u32) + 1
}

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn python_visibility(name: &str) -> Option<String> {
    // Dunders (e.g. __init__) are public per Python convention.
    if name.starts_with("__") && name.ends_with("__") && name.len() > 4 {
        return Some("public".into());
    }
    // Name-mangled (`__foo`) — private.
    if name.starts_with("__") {
        return Some("private".into());
    }
    // Single-underscore prefix → conventionally private.
    if name.starts_with('_') {
        return Some("private".into());
    }
    Some("public".into())
}

/// Build a dotted Python scope path for a symbol — walks up `class_definition`
/// ancestors, prepending their names. Top-level symbols get just their own name.
fn python_scope_path(node: Node<'_>, src: &str, name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.parent();
    while let Some(p) = cursor {
        if (p.kind() == "class_definition" || p.kind() == "function_definition")
            && let Some(n) = p.child_by_field_name("name")
        {
            parts.push(node_text(n, src).to_string());
        }
        cursor = p.parent();
    }
    parts.reverse();
    parts.push(name.to_string());
    parts.join(".")
}

/// Depth = number of enclosing class/function scopes.
fn python_scope_depth(node: Node<'_>) -> u32 {
    let mut depth: u32 = 0;
    let mut cursor = node.parent();
    while let Some(p) = cursor {
        if p.kind() == "class_definition" || p.kind() == "function_definition" {
            depth = depth.saturating_add(1);
        }
        cursor = p.parent();
    }
    depth
}

fn is_screaming_snake(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut has_upper = false;
    for c in name.chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
            if c.is_ascii_uppercase() {
                has_upper = true;
            }
        } else {
            return false;
        }
    }
    has_upper
}

impl LanguageBackend for PythonBackend {
    fn language_name(&self) -> &'static str {
        "python"
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = symbol_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Symbol> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                match cap_name {
                    "class.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let signature = first_line(content, node);
                            let generic_params = type_mapper::generics_for_class(node, content);
                            let scope_path = python_scope_path(node, content, &name);
                            let scope_depth = python_scope_depth(node);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Class,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: python_visibility(&name),
                                signature: Some(signature),
                                name,
                                generic_params,
                                scope_path: Some(scope_path),
                                scope_depth: Some(scope_depth),
                                ..Default::default()
                            });
                        }
                    }
                    "fn.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let signature = first_line(content, node);
                            let parameters = node
                                .child_by_field_name("parameters")
                                .map(|p| type_mapper::parameters_from_node(p, content))
                                .unwrap_or_default();
                            let return_type = Some(type_mapper::return_type_from_node(
                                node.child_by_field_name("return_type"),
                                content,
                            ));
                            let effects = type_mapper::effects_for_function(node, content);
                            let scope_path = python_scope_path(node, content, &name);
                            let scope_depth = python_scope_depth(node);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: python_visibility(&name),
                                signature: Some(signature),
                                name,
                                parameters,
                                return_type,
                                effects,
                                scope_path: Some(scope_path),
                                scope_depth: Some(scope_depth),
                                ..Default::default()
                            });
                        }
                    }
                    "const.name" => {
                        let name = node_text(node, content).to_string();
                        if is_screaming_snake(&name) {
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Const,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: python_visibility(&name),
                                signature: Some(name.clone()),
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = import_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Import> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                let line = line_of(node);
                match cap_name {
                    "import.stmt" => walk_import_statement(node, content, line, &mut out),
                    "from.stmt" => walk_from_statement(node, content, line, &mut out),
                    _ => {}
                }
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = ref_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<SymbolReference> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                let target_raw = node_text(node, content).to_string();
                if target_raw.is_empty() {
                    continue;
                }
                let kind = match cap_name {
                    "call.name" | "call.method" => SymbolRefKind::Call,
                    "inherit.name" => SymbolRefKind::Inherit,
                    _ => continue,
                };
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw,
                    ref_kind: kind,
                    source_line: line_of(node),
                });
            }
        }
        // Type references — Python's typed_parameter / return_type / type
        // annotations are subtypes that aren't easily captured by tree-sitter
        // queries above due to nested-pattern restrictions. Walk the tree
        // directly for those.
        collect_type_references(tree.root_node(), content, &mut out);
        out
    }

    fn extract_function_metrics(&self, content: &str) -> Vec<FunctionMetrics> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let mut out: Vec<FunctionMetrics> = Vec::new();
        collect_function_metrics(tree.root_node(), content, &mut out);
        out
    }
}

/// Walk every `function_definition` in the tree and score it.
fn collect_function_metrics(node: Node<'_>, src: &str, out: &mut Vec<FunctionMetrics>) {
    if node.kind() == "function_definition"
        && let Some(name_node) = node.child_by_field_name("name")
        && let Some(body) = node.child_by_field_name("body")
    {
        let name = node_text(name_node, src).to_string();
        out.push(score_python_function(
            &name,
            line_of(node),
            end_line_of(body),
            body,
            src,
            &name,
        ));
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_function_metrics(child, src, out);
    }
}

/// Score one Python function body.
fn score_python_function(
    name: &str,
    start_line: u32,
    end_line: u32,
    body: Node<'_>,
    src: &str,
    fn_name: &str,
) -> FunctionMetrics {
    use std::collections::HashMap;
    let mut decision_points: u32 = 0;
    let mut cognitive_increments: Vec<CognitiveIncrement> = Vec::new();
    let mut operators: HashMap<&'static str, u32> = HashMap::new();
    let mut operands: HashMap<String, u32> = HashMap::new();
    let mut npath_factors: Vec<u64> = Vec::new();
    let mut panic_paths: u32 = 0;

    walk_python_body(
        body,
        src,
        0,
        &mut decision_points,
        &mut cognitive_increments,
        &mut operators,
        &mut operands,
        &mut npath_factors,
        &mut panic_paths,
        fn_name,
    );

    let source_lines = end_line.saturating_sub(start_line) + 1;
    let input = ScoringInput {
        name,
        start_line,
        end_line,
        decision_points,
        cognitive_increments,
        operators,
        operands,
        npath_factors,
        source_lines,
        comment_lines: 0, // not counted in tree-sitter pass
        panic_paths,
        unsafe_blocks: 0,
    };
    complexity::score(&input)
}

/// Static set of Python operator/keyword tokens (η1 universe).
const PYTHON_OPERATOR_KINDS: &[&str] = &[
    // Punctuation kinds tree-sitter emits as their literal text:
    "+", "-", "*", "/", "%", "//", "**", "==", "!=", "<", ">", "<=", ">=", "=", "+=", "-=", "*=",
    "/=", "//=", "**=", "%=", "&", "|", "^", "<<", ">>", "~", ".", ",", ":", ";", "(", ")", "[",
    "]", "{", "}", "@", "->", "and", "or", "not", "if", "elif", "else", "while", "for", "in", "is",
    "lambda", "def", "class", "return", "yield", "raise", "try", "except", "finally", "with", "as",
    "import", "from", "pass", "break", "continue", "global", "nonlocal", "assert", "del",
];

#[allow(clippy::too_many_arguments)]
fn walk_python_body(
    node: Node<'_>,
    src: &str,
    depth: u8,
    decision_points: &mut u32,
    cognitive_increments: &mut Vec<CognitiveIncrement>,
    operators: &mut std::collections::HashMap<&'static str, u32>,
    operands: &mut std::collections::HashMap<String, u32>,
    npath_factors: &mut Vec<u64>,
    panic_paths: &mut u32,
    fn_name: &str,
) {
    let kind = node.kind();

    // Classify leaf tokens for Halstead.
    if node.child_count() == 0 {
        let text = node_text(node, src);
        if !text.is_empty() {
            if let Some(op) = match_python_operator(text) {
                *operators.entry(op).or_insert(0) += 1;
            } else if matches!(
                kind,
                "identifier" | "integer" | "float" | "string" | "true" | "false" | "none"
            ) {
                *operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
    }

    // Decision points & cognitive increments.
    let mut new_depth = depth;
    let mut entered_nest = false;
    match kind {
        "if_statement" | "elif_clause" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(2);
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "while_statement" | "for_statement" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(2);
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "except_clause" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(2);
        }
        "conditional_expression" => {
            *decision_points = decision_points.saturating_add(1);
            npath_factors.push(2);
        }
        "boolean_operator" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::LogicalSequence,
            });
            npath_factors.push(2);
        }
        "case_clause" => {
            *decision_points = decision_points.saturating_add(1);
            npath_factors.push(2);
        }
        "raise_statement" => {
            *panic_paths = panic_paths.saturating_add(1);
        }
        "assert_statement" => {
            *panic_paths = panic_paths.saturating_add(1);
        }
        "break_statement" | "continue_statement" => {
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::BreakInFlow,
            });
        }
        "call" => {
            // Recursion detection: call expression whose function is an
            // identifier matching the enclosing function name.
            if let Some(func) = node.child_by_field_name("function") {
                let name = node_text(func, src);
                if name == fn_name {
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::Recursion,
                    });
                }
            }
        }
        // Don't recurse into nested function bodies — they get their own
        // metrics row from `collect_function_metrics`. Same for class bodies
        // since methods are also `function_definition` nodes.
        "function_definition" => {
            return;
        }
        _ => {}
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        walk_python_body(
            child,
            src,
            new_depth,
            decision_points,
            cognitive_increments,
            operators,
            operands,
            npath_factors,
            panic_paths,
            fn_name,
        );
    }
    if entered_nest {
        // depth restoration handled by caller's local copy; no-op
    }
}

fn match_python_operator(s: &str) -> Option<&'static str> {
    PYTHON_OPERATOR_KINDS.iter().copied().find(|t| *t == s)
}

/// Slice the first line of a node from `content`.
fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    // Search forward for newline or `:` to mark end of header.
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b':' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Walk an `import_statement` and emit one `Import` per dotted name (or alias).
fn walk_import_statement(node: Node<'_>, src: &str, line: u32, out: &mut Vec<Import>) {
    // children: "import" then comma-separated dotted_name | aliased_import
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let target_raw = node_text(child, src).to_string();
                out.push(Import {
                    target_raw,
                    source_line: line,
                    alias: None,
                });
            }
            "aliased_import" => {
                let mut name = String::new();
                let mut alias: Option<String> = None;
                let mut walker = child.walk();
                for c in child.named_children(&mut walker) {
                    match c.kind() {
                        "dotted_name" => name = node_text(c, src).to_string(),
                        "identifier" => alias = Some(node_text(c, src).to_string()),
                        _ => {}
                    }
                }
                if !name.is_empty() {
                    out.push(Import {
                        target_raw: name,
                        source_line: line,
                        alias,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Walk a `from x import a, b` (or `from . import s`, or `from .pkg import a`).
fn walk_from_statement(node: Node<'_>, src: &str, line: u32, out: &mut Vec<Import>) {
    // module_name node: dotted_name OR relative_import (which contains dots + optional dotted_name)
    let module_prefix = match node.child_by_field_name("module_name") {
        Some(mn) => render_from_module(mn, src),
        None => String::new(),
    };
    // Imported names: walk ALL children that are not "from"/"import"/etc keywords.
    let mut cursor = node.walk();
    let mut seen_import = false;
    for child in node.children(&mut cursor) {
        // `import` keyword resets — names follow it.
        let kind = child.kind();
        if kind == "import" {
            seen_import = true;
            continue;
        }
        if !seen_import {
            continue;
        }
        match kind {
            "dotted_name" | "identifier" => {
                let leaf = node_text(child, src).to_string();
                if leaf.is_empty() {
                    continue;
                }
                let target_raw = if module_prefix.is_empty() {
                    leaf
                } else if module_prefix.ends_with('.') {
                    // Pure relative `from . import sibling` → `.sibling`.
                    format!("{}{}", module_prefix, leaf)
                } else {
                    format!("{}.{}", module_prefix, leaf)
                };
                out.push(Import {
                    target_raw,
                    source_line: line,
                    alias: None,
                });
            }
            "aliased_import" => {
                let mut leaf = String::new();
                let mut alias: Option<String> = None;
                let mut walker = child.walk();
                for c in child.named_children(&mut walker) {
                    match c.kind() {
                        "dotted_name" | "identifier" => {
                            if leaf.is_empty() {
                                leaf = node_text(c, src).to_string();
                            } else {
                                alias = Some(node_text(c, src).to_string());
                            }
                        }
                        _ => {}
                    }
                }
                if leaf.is_empty() {
                    continue;
                }
                let target_raw = if module_prefix.is_empty() {
                    leaf
                } else if module_prefix.ends_with('.') {
                    format!("{}{}", module_prefix, leaf)
                } else {
                    format!("{}.{}", module_prefix, leaf)
                };
                out.push(Import {
                    target_raw,
                    source_line: line,
                    alias,
                });
            }
            _ => {}
        }
    }
}

/// Convert a module-name node (`dotted_name` or `relative_import`) into the
/// canonical prefix string. Relative imports preserve leading dots and end
/// with `.` only when there's no module portion (`from . import x`).
fn render_from_module(node: Node<'_>, src: &str) -> String {
    match node.kind() {
        "dotted_name" => node_text(node, src).to_string(),
        "relative_import" => {
            // Children: import_prefix (dots) + optional dotted_name.
            let mut dots = String::new();
            let mut name = String::new();
            let mut walker = node.walk();
            for c in node.named_children(&mut walker) {
                match c.kind() {
                    "import_prefix" => dots = node_text(c, src).to_string(),
                    "dotted_name" => name = node_text(c, src).to_string(),
                    _ => {}
                }
            }
            if name.is_empty() {
                // `from . import x` → prefix = "."
                dots
            } else {
                format!("{}{}", dots, name)
            }
        }
        _ => node_text(node, src).to_string(),
    }
}

/// Walk the tree once for type annotations and parameter types — these are
/// awkward to capture via S-expression queries in this grammar version.
fn collect_type_references(root: Node<'_>, src: &str, out: &mut Vec<SymbolReference>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut walker = node.walk();
        for child in node.children(&mut walker) {
            stack.push(child);
        }
        // Match: parameter `:` `type` block or function return annotation.
        if node.kind() == "type"
            && let Some(inner) = node.named_child(0)
            && inner.kind() == "identifier"
        {
            let target_raw = node_text(inner, src).to_string();
            if !target_raw.is_empty() {
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw,
                    ref_kind: SymbolRefKind::TypeUse,
                    source_line: line_of(inner),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "from os.path import join as pjoin\n\
from . import sibling\n\
import collections\n\
\n\
CONSTANT = 42\n\
\n\
class Foo(Base):\n\
    def method(self, x: int) -> str:\n\
        return pjoin(str(x))\n\
\n\
def helper():\n\
    Foo().method(1)\n\
";

    #[test]
    fn extract_symbols_finds_class_function_and_const() {
        let syms = PYTHON_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "names: {:?}", names);
        assert!(names.contains(&"method"));
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"CONSTANT"));
        let foo = syms.iter().find(|s| s.name == "Foo").unwrap();
        assert_eq!(foo.kind, SymbolKind::Class);
        let constant = syms.iter().find(|s| s.name == "CONSTANT").unwrap();
        assert_eq!(constant.kind, SymbolKind::Const);
    }

    #[test]
    fn extract_imports_with_alias_and_relative() {
        let imps = PYTHON_BACKEND.extract_imports(SAMPLE);
        let by_target = |t: &str| imps.iter().find(|i| i.target_raw == t);
        // from os.path import join as pjoin → target=os.path.join, alias=pjoin
        let pjoin = by_target("os.path.join").expect("os.path.join");
        assert_eq!(pjoin.alias.as_deref(), Some("pjoin"));
        // from . import sibling → target=.sibling, no alias
        assert!(by_target(".sibling").is_some(), "imports: {:?}", imps);
        // import collections → target=collections, no alias
        assert!(by_target("collections").is_some());
    }

    #[test]
    fn extract_references_finds_calls_inheritance_and_types() {
        let refs = PYTHON_BACKEND.extract_references(SAMPLE);
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Call)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(calls.contains(&"pjoin"), "calls: {:?}", calls);
        assert!(calls.contains(&"str"));
        // class Foo(Base): → Inherit
        let inherits: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Inherit)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(inherits.contains(&"Base"), "inherits: {:?}", inherits);
        // : int / -> str → TypeUse
        let types: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::TypeUse)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(types.contains(&"int"), "types: {:?}", types);
        assert!(types.contains(&"str"));
    }

    #[test]
    fn relative_pkg_emits_dot_prefixed_target() {
        let src = "from .pkg import a, b\n";
        let imps = PYTHON_BACKEND.extract_imports(src);
        let targets: Vec<&str> = imps.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(targets.contains(&".pkg.a"), "imports: {:?}", imps);
        assert!(targets.contains(&".pkg.b"));
    }

    #[test]
    fn parse_error_or_empty_yields_no_panic() {
        // Tree-sitter is error-tolerant; even garbage parses to a tree with
        // ERROR nodes. Just assert no panic on common edge cases.
        let cases = ["", "   \n\n", "this is not python {", "def\nclass"];
        for c in cases {
            let _ = PYTHON_BACKEND.extract_symbols(c);
            let _ = PYTHON_BACKEND.extract_imports(c);
            let _ = PYTHON_BACKEND.extract_references(c);
        }
    }

    #[test]
    fn private_naming_convention() {
        assert_eq!(python_visibility("foo"), Some("public".into()));
        assert_eq!(python_visibility("_foo"), Some("private".into()));
        assert_eq!(python_visibility("__foo"), Some("private".into()));
        assert_eq!(python_visibility("__init__"), Some("public".into()));
    }

    #[test]
    fn screaming_snake_check() {
        assert!(is_screaming_snake("CONSTANT"));
        assert!(is_screaming_snake("MAX_SIZE_2"));
        assert!(!is_screaming_snake("foo"));
        assert!(!is_screaming_snake("Foo"));
        assert!(!is_screaming_snake(""));
    }

    #[test]
    fn language_name_is_python() {
        assert_eq!(PYTHON_BACKEND.language_name(), "python");
    }

    // ========================================================================
    // extract_function_metrics tests (SOTA Phase 1, A1)
    // ========================================================================

    #[test]
    fn py_empty_function_has_cc_one() {
        let src = "def empty():\n    pass\n";
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "empty");
        assert_eq!(m[0].cyclomatic, 1);
    }

    #[test]
    fn py_if_elif_else_counts_branches() {
        let src = r#"
def classify(x):
    if x > 0:
        return 1
    elif x < 0:
        return -1
    else:
        return 0
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        // if (+1) + elif (+1) = 2 decisions → CC = 3
        assert_eq!(m[0].cyclomatic, 3);
    }

    #[test]
    fn py_for_loop_counts() {
        let src = r#"
def total(xs):
    s = 0
    for x in xs:
        s += x
    return s
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        assert_eq!(m[0].cyclomatic, 2);
    }

    #[test]
    fn py_try_except() {
        let src = r#"
def parse(s):
    try:
        return int(s)
    except ValueError:
        return None
    except TypeError:
        return None
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        // 2 except clauses = 2 decisions → CC = 3
        assert_eq!(m[0].cyclomatic, 3);
    }

    #[test]
    fn py_ternary_counts_as_decision() {
        let src = r#"
def abs_val(x):
    return x if x >= 0 else -x
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        assert!(m[0].cyclomatic >= 2);
    }

    #[test]
    fn py_raise_counts_as_panic_path() {
        let src = r#"
def must_be_positive(x):
    if x <= 0:
        raise ValueError("must be positive")
    return x
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        assert_eq!(m[0].panic_paths, 1);
    }

    #[test]
    fn py_method_in_class_extracted() {
        let src = r#"
class C:
    def method(self, x):
        if x > 0:
            return 1
        return 0
"#;
        let m = PYTHON_BACKEND.extract_function_metrics(src);
        let names: Vec<&str> = m.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"method"));
        let method = m.iter().find(|f| f.name == "method").expect("method");
        assert_eq!(method.cyclomatic, 2);
    }

    #[test]
    fn py_invalid_source_yields_empty_metrics() {
        // tree-sitter is tolerant — try a minimal invalid form
        let m = PYTHON_BACKEND.extract_function_metrics("def\n  oops");
        // The tree-sitter parser may produce a partial tree; either we get
        // 0 functions or the partial ones have safe (low) metrics.
        for fm in &m {
            assert!(
                fm.cyclomatic <= 5,
                "spurious metrics in invalid source: {:?}",
                fm
            );
        }
    }
}
