//! Java language backend (Tier 0e). Uses `tree-sitter-java`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "java/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static JAVA_BACKEND: JavaBackend = JavaBackend;
pub struct JavaBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("set_language java");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(package_declaration) @pkg.def

(class_declaration name: (identifier) @class.name) @class.def
(interface_declaration name: (identifier) @iface.name) @iface.def
(enum_declaration name: (identifier) @enum.name) @enum.def
(record_declaration name: (identifier) @record.name) @record.def

(method_declaration name: (identifier) @method.name) @method.def

(field_declaration
  (variable_declarator name: (identifier) @field.name)) @field.def
"#;

const IMPORT_QUERY: &str = r#"
(import_declaration (scoped_identifier) @import.target) @import.decl
"#;

const REF_QUERY: &str = r#"
(method_invocation name: (identifier) @ref.call)
(object_creation_expression type: (type_identifier) @ref.call)

(superclass (type_identifier) @ref.inherit)
(super_interfaces (type_list (type_identifier) @ref.impl))
(extends_interfaces (type_list (type_identifier) @ref.inherit))

(formal_parameter type: (type_identifier) @ref.type)
(method_declaration type: (type_identifier) @ref.type)
(field_declaration type: (type_identifier) @ref.type)
(local_variable_declaration type: (type_identifier) @ref.type)
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_java::LANGUAGE.into(), SYMBOL_QUERY).expect("symbol query java")
    })
}
fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_java::LANGUAGE.into(), IMPORT_QUERY).expect("import query java")
    })
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_java::LANGUAGE.into(), REF_QUERY).expect("ref query java")
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

/// Render a `scoped_identifier` (or any dotted name) as a simple dotted string.
fn dotted_name_of(node: Node<'_>, src: &str) -> String {
    // tree-sitter-java represents `a.b.c` as a left-recursive scoped_identifier
    // chain. The simplest correct rendering is just the node's source text;
    // it's already in `a.b.c` form.
    node_text(node, src).to_string()
}

/// Walk the optional `modifiers` child of an item node and return the visibility
/// string. Default (none of the three explicit keywords) → "module" (package-private).
fn parse_visibility(modifiers: Option<Node<'_>>) -> Option<String> {
    let m = modifiers?;
    let mut walker = m.walk();
    for child in m.children(&mut walker) {
        match child.kind() {
            "public" => return Some("public".into()),
            "protected" => return Some("protected".into()),
            "private" => return Some("private".into()),
            _ => {}
        }
    }
    Some("module".into())
}

/// Test whether a `modifiers` node contains both `static` and `final` keywords.
fn is_static_final(modifiers: Option<Node<'_>>) -> bool {
    let Some(m) = modifiers else {
        return false;
    };
    let mut walker = m.walk();
    let mut has_static = false;
    let mut has_final = false;
    for child in m.children(&mut walker) {
        match child.kind() {
            "static" => has_static = true,
            "final" => has_final = true,
            _ => {}
        }
    }
    has_static && has_final
}

/// First non-anonymous child whose kind matches "modifiers".
fn modifiers_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut walker = node.walk();
    node.children(&mut walker).find(|c| c.kind() == "modifiers")
}

/// Truncate at first `{`, `;`, or newline.
fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'{' && bytes[end] != b';' && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

impl LanguageBackend for JavaBackend {
    fn language_name(&self) -> &'static str {
        "java"
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
                    "pkg.def" => {
                        // Find the scoped_identifier (or identifier) child.
                        if let Some(name_node) = node
                            .child_by_field_name("name")
                            .or_else(|| node.named_child(0))
                        {
                            let name = dotted_name_of(name_node, content);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Module,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: Some(name.clone()),
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "class.def" | "iface.def" | "enum.def" | "record.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let kind = match cap_name {
                                "iface.def" => SymbolKind::Interface,
                                "enum.def" => SymbolKind::Enum,
                                _ => SymbolKind::Class,
                            };
                            let visibility = parse_visibility(modifiers_child(node))
                                .or_else(|| Some("module".into()));
                            out.push(Symbol {
                                file_id: 0,
                                kind,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility,
                                signature: Some(first_line(content, node)),
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "method.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let visibility = parse_visibility(modifiers_child(node))
                                .or_else(|| Some("module".into()));
                            // Shadow-ASR extraction for the method.
                            let parameters = node
                                .child_by_field_name("parameters")
                                .map(|p| type_mapper::parameters_from_node(p, content))
                                .unwrap_or_default();
                            let return_type =
                                Some(type_mapper::return_type_from_method(node, content));
                            let generic_params = type_mapper::generics_for_method(node, content);
                            let effects = type_mapper::effects_for_method(node, content);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility,
                                signature: Some(first_line(content, node)),
                                name,
                                parameters,
                                return_type,
                                generic_params,
                                effects,
                                scope_depth: Some(1),
                                ..Default::default()
                            });
                        }
                    }
                    "field.def" => {
                        // Only emit Const for `static final` fields.
                        if !is_static_final(modifiers_child(node)) {
                            continue;
                        }
                        // Walk for variable_declarator name.
                        let mut walker = node.walk();
                        for child in node.named_children(&mut walker) {
                            if child.kind() == "variable_declarator"
                                && let Some(name_node) = child.child_by_field_name("name")
                            {
                                let name = node_text(name_node, content).to_string();
                                let visibility = parse_visibility(modifiers_child(node))
                                    .or_else(|| Some("module".into()));
                                out.push(Symbol {
                                    file_id: 0,
                                    kind: SymbolKind::Const,
                                    start_line: line_of(name_node),
                                    end_line: end_line_of(name_node),
                                    parent_id: None,
                                    visibility,
                                    signature: Some(name.clone()),
                                    name,
                                    ..Default::default()
                                });
                            }
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
            // Each match has @import.target and @import.decl. Compose target and
            // detect wildcard / static.
            let target_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.target");
            let decl_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.decl");
            let (Some(target), Some(decl)) = (target_cap, decl_cap) else {
                continue;
            };
            let mut target_raw = dotted_name_of(target.node, content);
            // Detect wildcard via anonymous `*` child of import_declaration.
            let mut walker = decl.node.walk();
            let mut is_wildcard = false;
            for child in decl.node.children(&mut walker) {
                if child.kind() == "*" || child.kind() == "asterisk" {
                    is_wildcard = true;
                    break;
                }
            }
            if is_wildcard {
                target_raw.push_str(".*");
            }
            out.push(Import {
                target_raw,
                source_line: line_of(decl.node),
                alias: None,
            });
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
                    "ref.call" => SymbolRefKind::Call,
                    "ref.inherit" => SymbolRefKind::Inherit,
                    "ref.impl" => SymbolRefKind::Impl,
                    "ref.type" => SymbolRefKind::TypeUse,
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
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
package com.example;
import java.util.List;
import java.util.Map;
import static java.util.Collections.emptyList;

public class Greeter implements Hello {
    public static final int MAX = 100;
    private String prefix;
    public String greet(String name) { return prefix + name; }
}

interface Hello {
    String greet(String name);
}
"#;

    #[test]
    fn extract_symbols_finds_package_class_interface_methods_const() {
        let syms = JAVA_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"com.example"), "names: {:?}", names);
        assert!(names.contains(&"Greeter"));
        assert!(names.contains(&"Hello"));
        assert!(names.contains(&"MAX"));
        assert!(names.contains(&"greet"));
        let max = syms.iter().find(|s| s.name == "MAX").unwrap();
        assert_eq!(max.kind, SymbolKind::Const);
        let hello = syms.iter().find(|s| s.name == "Hello").unwrap();
        assert_eq!(hello.kind, SymbolKind::Interface);
        let greeter = syms.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Class);
    }

    #[test]
    fn extract_imports_emits_three_rows() {
        let imps = JAVA_BACKEND.extract_imports(SAMPLE);
        let targets: Vec<&str> = imps.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(targets.contains(&"java.util.List"));
        assert!(targets.contains(&"java.util.Map"));
        assert!(targets.contains(&"java.util.Collections.emptyList"));
    }

    #[test]
    fn wildcard_import_emits_dot_star() {
        let src = "import java.util.*;\n";
        let imps = JAVA_BACKEND.extract_imports(src);
        let targets: Vec<&str> = imps.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(targets.contains(&"java.util.*"), "imports: {:?}", imps);
    }

    #[test]
    fn extract_references_finds_implements_and_types() {
        let refs = JAVA_BACKEND.extract_references(SAMPLE);
        let impls: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Impl)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(impls.contains(&"Hello"), "impls: {:?}", impls);
        let types: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::TypeUse)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(types.contains(&"String"), "types: {:?}", types);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "package ;", "class { {"] {
            let _ = JAVA_BACKEND.extract_symbols(s);
            let _ = JAVA_BACKEND.extract_imports(s);
            let _ = JAVA_BACKEND.extract_references(s);
        }
    }

    #[test]
    fn language_name_is_java() {
        assert_eq!(JAVA_BACKEND.language_name(), "java");
    }
}
