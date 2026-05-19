//! Python language backend (Tier 0e). Uses `tree-sitter-python` for symbols,
//! imports, and references.
//!
//! `tree-sitter::Parser` is `!Sync`, so we keep one per thread via
//! `thread_local!`. `Query` is `Send + Sync`, so we cache the three queries
//! once per process via `OnceLock`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::backend::LanguageBackend;
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
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Class,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: python_visibility(&name),
                                signature: Some(signature),
                                name,
                            });
                        }
                    }
                    "fn.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let signature = first_line(content, node);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: python_visibility(&name),
                                signature: Some(signature),
                                name,
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
}
