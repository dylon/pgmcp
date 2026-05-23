//! Scala language backend (Tier 0e). Uses `tree-sitter-scala`.
//!
//! Handles both Scala 2 and Scala 3 surface syntax. Scala 3-specific features
//! (enum, given/using, indented syntax, top-level definitions) are supported
//! by the grammar; extension methods become free `Function` rows without
//! parent class context.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "scala/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static SCALA_BACKEND: ScalaBackend = ScalaBackend;
pub struct ScalaBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_scala::LANGUAGE.into())
            .expect("set_language scala");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(package_clause name: (package_identifier) @pkg.name) @pkg.def

(class_definition name: (identifier) @class.name) @class.def
(object_definition name: (identifier) @object.name) @object.def
(trait_definition name: (identifier) @trait.name) @trait.def
(enum_definition name: (identifier) @enum.name) @enum.def

(function_definition name: (identifier) @fn.name) @fn.def
(function_declaration name: (identifier) @fn.name) @fn.def

(val_definition pattern: (identifier) @val.name) @val.def
"#;

const IMPORT_QUERY: &str = r#"
(import_declaration) @import.decl
"#;

const REF_QUERY: &str = r#"
(call_expression function: (identifier) @ref.call)
(extends_clause) @extends.clause
(type_identifier) @ref.type
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_scala::LANGUAGE.into(), SYMBOL_QUERY).expect("symbol query scala")
    })
}
fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_scala::LANGUAGE.into(), IMPORT_QUERY).expect("import query scala")
    })
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_scala::LANGUAGE.into(), REF_QUERY).expect("ref query scala")
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

fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'{' && bytes[end] != b'=' && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Extract a `type_identifier` text from a possibly-wrapped type expression.
/// Walks through `generic_type`, `stable_type_identifier`, etc. to find the
/// rightmost / leaf `type_identifier`.
fn unwrap_type_name(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() == "type_identifier" {
        return Some(node_text(node, src).to_string());
    }
    // Try named children recursively, taking the LAST type_identifier we
    // encounter (rightmost segment of qualified types).
    let mut last: Option<String> = None;
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if let Some(name) = unwrap_type_name(child, src) {
            last = Some(name);
        }
    }
    last
}

impl LanguageBackend for ScalaBackend {
    fn language_name(&self) -> &'static str {
        "scala"
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
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
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
                    "class.def" | "object.def" | "trait.def" | "enum.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let kind = match cap_name {
                                "trait.def" => SymbolKind::Trait,
                                "enum.def" => SymbolKind::Enum,
                                _ => SymbolKind::Class,
                            };
                            out.push(Symbol {
                                file_id: 0,
                                kind,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: Some(first_line(content, node)),
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "fn.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            let parameters = node
                                .child_by_field_name("parameters")
                                .map(|p| type_mapper::parameters_from_node(p, content))
                                .unwrap_or_default();
                            let return_type =
                                Some(type_mapper::return_type_from_function(node, content));
                            let generic_params = type_mapper::generics_for_function(node, content);
                            let effects = type_mapper::effects_for_function(node, content);
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: Some(first_line(content, node)),
                                name,
                                parameters,
                                return_type,
                                generic_params,
                                effects,
                                scope_depth: Some(0),
                                ..Default::default()
                            });
                        }
                    }
                    "val.def" => {
                        if let Some(name_node) = node.child_by_field_name("pattern") {
                            // Only emit Const for typed top-level vals to match
                            // the semantics described in the design.
                            let has_type = node.child_by_field_name("type").is_some();
                            if !has_type {
                                continue;
                            }
                            let name = node_text(name_node, content).to_string();
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Const,
                                start_line: line_of(name_node),
                                end_line: end_line_of(name_node),
                                parent_id: None,
                                visibility: Some("public".into()),
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
                if cap_name != "import.decl" {
                    continue;
                }
                walk_import_decl(cap.node, content, &mut out);
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
                match cap_name {
                    "ref.call" => {
                        let target_raw = node_text(node, content).to_string();
                        if !target_raw.is_empty() {
                            out.push(SymbolReference {
                                source_file_id: 0,
                                source_symbol_id: None,
                                target_file_id: None,
                                target_symbol_id: None,
                                target_raw,
                                ref_kind: SymbolRefKind::Call,
                                source_line: line_of(node),
                            });
                        }
                    }
                    "ref.type" => {
                        let target_raw = node_text(node, content).to_string();
                        if !target_raw.is_empty() {
                            out.push(SymbolReference {
                                source_file_id: 0,
                                source_symbol_id: None,
                                target_file_id: None,
                                target_symbol_id: None,
                                target_raw,
                                ref_kind: SymbolRefKind::TypeUse,
                                source_line: line_of(node),
                            });
                        }
                    }
                    "extends.clause" => {
                        // Walk children; first type → Inherit, rest → Impl.
                        let mut walker = node.walk();
                        let mut first = true;
                        for child in node.named_children(&mut walker) {
                            if let Some(name) = unwrap_type_name(child, content) {
                                let kind = if first {
                                    SymbolRefKind::Inherit
                                } else {
                                    SymbolRefKind::Impl
                                };
                                out.push(SymbolReference {
                                    source_file_id: 0,
                                    source_symbol_id: None,
                                    target_file_id: None,
                                    target_symbol_id: None,
                                    target_raw: name,
                                    ref_kind: kind,
                                    source_line: line_of(child),
                                });
                                first = false;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

/// Walk a Scala `import_declaration` node and emit `Import` rows.
///
/// The grammar puts the dotted path's identifiers as direct children. After
/// the path identifiers, a final node may be:
/// - nothing (`import a.b` → emit one row, target=`a.b`)
/// - `namespace_wildcard` for `_` / `*` / `given`
/// - `namespace_selectors` for `{C, D => MSet, given Foo}`
/// - `as_renamed_identifier` for `import x as y`
fn walk_import_decl(node: Node<'_>, src: &str, out: &mut Vec<Import>) {
    let line = line_of(node);
    // Collect path identifiers and trailing structure.
    let mut walker = node.walk();
    let mut prefix: Vec<String> = Vec::new();
    let mut trailing: Option<Node<'_>> = None;
    for child in node.named_children(&mut walker) {
        match child.kind() {
            "identifier" => prefix.push(node_text(child, src).to_string()),
            "namespace_selectors"
            | "namespace_wildcard"
            | "as_renamed_identifier"
            | "arrow_renamed_identifier" => {
                trailing = Some(child);
            }
            _ => {}
        }
    }
    let prefix_str = prefix.join(".");
    match trailing {
        None => {
            // `import a.b` — target = full path; the last path component is the leaf.
            if !prefix.is_empty() {
                out.push(Import {
                    target_raw: prefix_str,
                    source_line: line,
                    alias: None,
                });
            }
        }
        Some(t) => match t.kind() {
            "namespace_wildcard" => {
                let wildcard = node_text(t, src).to_string();
                let target_raw = if prefix.is_empty() {
                    wildcard
                } else {
                    format!("{}.{}", prefix_str, wildcard)
                };
                out.push(Import {
                    target_raw,
                    source_line: line,
                    alias: None,
                });
            }
            "namespace_selectors" => {
                let mut sw = t.walk();
                for sel in t.named_children(&mut sw) {
                    match sel.kind() {
                        "identifier" => {
                            let leaf = node_text(sel, src).to_string();
                            let target_raw = if prefix.is_empty() {
                                leaf
                            } else {
                                format!("{}.{}", prefix_str, leaf)
                            };
                            out.push(Import {
                                target_raw,
                                source_line: line,
                                alias: None,
                            });
                        }
                        "arrow_renamed_identifier" | "as_renamed_identifier" => {
                            let mut leaf: Option<String> = None;
                            let mut alias: Option<String> = None;
                            let mut rw = sel.walk();
                            for c in sel.named_children(&mut rw) {
                                if c.kind() == "identifier" {
                                    if leaf.is_none() {
                                        leaf = Some(node_text(c, src).to_string());
                                    } else {
                                        alias = Some(node_text(c, src).to_string());
                                    }
                                }
                            }
                            if let Some(l) = leaf {
                                let target_raw = if prefix.is_empty() {
                                    l
                                } else {
                                    format!("{}.{}", prefix_str, l)
                                };
                                out.push(Import {
                                    target_raw,
                                    source_line: line,
                                    alias,
                                });
                            }
                        }
                        "namespace_wildcard" => {
                            let w = node_text(sel, src).to_string();
                            let target_raw = if prefix.is_empty() {
                                w
                            } else {
                                format!("{}.{}", prefix_str, w)
                            };
                            out.push(Import {
                                target_raw,
                                source_line: line,
                                alias: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "as_renamed_identifier" | "arrow_renamed_identifier" => {
                // Top-level `import a.b as c` form.
                let mut leaf: Option<String> = None;
                let mut alias: Option<String> = None;
                let mut rw = t.walk();
                for c in t.named_children(&mut rw) {
                    if c.kind() == "identifier" {
                        if leaf.is_none() {
                            leaf = Some(node_text(c, src).to_string());
                        } else {
                            alias = Some(node_text(c, src).to_string());
                        }
                    }
                }
                if let Some(l) = leaf {
                    let target_raw = if prefix.is_empty() {
                        l
                    } else {
                        format!("{}.{}", prefix_str, l)
                    };
                    out.push(Import {
                        target_raw,
                        source_line: line,
                        alias,
                    });
                }
            }
            _ => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
package com.example

import scala.collection.mutable.{Map, Set => MSet}
import java.util._

trait Greeter {
  def greet(name: String): String
}

class HelloGreeter(prefix: String) extends Greeter {
  def greet(name: String): String = prefix + name
}

object Constants {
  val MAX: Int = 100
}
"#;

    #[test]
    fn extract_symbols_finds_package_classes_methods() {
        let syms = SCALA_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"com.example"), "names: {:?}", names);
        assert!(names.contains(&"Greeter"));
        assert!(names.contains(&"HelloGreeter"));
        assert!(names.contains(&"Constants"));
        assert!(names.contains(&"greet"));
        assert!(names.contains(&"MAX"));
        let trait_g = syms.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(trait_g.kind, SymbolKind::Trait);
        let max = syms.iter().find(|s| s.name == "MAX").unwrap();
        assert_eq!(max.kind, SymbolKind::Const);
    }

    #[test]
    fn extract_imports_handles_aliases_and_wildcards() {
        let imps = SCALA_BACKEND.extract_imports(SAMPLE);
        let pairs: Vec<(&str, Option<&str>)> = imps
            .iter()
            .map(|i| (i.target_raw.as_str(), i.alias.as_deref()))
            .collect();
        // {Map, Set => MSet} → two rows
        assert!(
            pairs.contains(&("scala.collection.mutable.Map", None)),
            "{:?}",
            pairs
        );
        assert!(
            pairs.contains(&("scala.collection.mutable.Set", Some("MSet"))),
            "{:?}",
            pairs
        );
        // import java.util._ → wildcard preserved
        assert!(pairs.iter().any(|(t, _)| t.starts_with("java.util.")));
    }

    #[test]
    fn extract_references_finds_inherit() {
        let refs = SCALA_BACKEND.extract_references(SAMPLE);
        let inherits: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Inherit)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(inherits.contains(&"Greeter"), "inherits: {:?}", inherits);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "package", "class { {"] {
            let _ = SCALA_BACKEND.extract_symbols(s);
            let _ = SCALA_BACKEND.extract_imports(s);
            let _ = SCALA_BACKEND.extract_references(s);
        }
    }

    #[test]
    fn language_name_is_scala() {
        assert_eq!(SCALA_BACKEND.language_name(), "scala");
    }
}
