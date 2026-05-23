//! TLA+ language backend using `tree-sitter-tlaplus`.
//!
//! Shadow-ASR contract: TLA+ is untyped — operators take arbitrary
//! expressions and produce values without static type annotations.
//! Symbols emitted here leave the shadow-ASR fields (`parameters`,
//! `return_type`, `generic_params`, `effects`, `type_tags`) at their
//! `Default::default()` values per the plan's per-language contract
//! (`~/.claude/plans/would-translating-the-asts-cosmic-quill.md` § Phase
//! C). Downstream tools degrade gracefully via LEFT JOIN + COALESCE.
//!
//! Symbol queries cover MODULE / operator-definitions / THEOREM / LEMMA /
//! VARIABLE(S) / CONSTANT(S). Import queries handle EXTENDS clauses.

#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static TLAPLUS_BACKEND: TlaPlusBackend = TlaPlusBackend;
pub struct TlaPlusBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_tlaplus::LANGUAGE.into())
            .expect("set_language tlaplus");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(module name: (identifier) @module.name) @module.def
(operator_definition name: (identifier) @op.name) @op.def
(function_definition name: (identifier) @fn.name) @fn.def
(theorem name: (identifier) @thm.name) @thm.def
(constant_declaration (identifier) @const.name)
(variable_declaration (identifier) @var.name)
"#;

const IMPORT_QUERY: &str = r#"
(extends (identifier_ref) @extends.name)
"#;

const REF_QUERY: &str = r#"
(bound_op name: (identifier_ref) @ref.name)
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_tlaplus::LANGUAGE.into(), SYMBOL_QUERY)
            .expect("symbol query tlaplus")
    })
}

fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_tlaplus::LANGUAGE.into(), IMPORT_QUERY)
            .expect("import query tlaplus")
    })
}

fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_tlaplus::LANGUAGE.into(), REF_QUERY).expect("ref query tlaplus")
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

impl LanguageBackend for TlaPlusBackend {
    fn language_name(&self) -> &'static str {
        "tlaplus"
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
                    "module.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Module,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: None,
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "op.def" | "fn.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: None,
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "thm.def" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility: Some("public".into()),
                                signature: None,
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "const.name" | "var.name" => {
                        let name = node_text(node, content).to_string();
                        out.push(Symbol {
                            file_id: 0,
                            kind: SymbolKind::Const,
                            start_line: line_of(node),
                            end_line: end_line_of(node),
                            parent_id: None,
                            visibility: Some("public".into()),
                            signature: None,
                            name,
                            ..Default::default()
                        });
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
                let node = cap.node;
                let target = node_text(node, content).to_string();
                if target.is_empty() {
                    continue;
                }
                out.push(Import {
                    target_raw: target,
                    source_line: line_of(node),
                    alias: None,
                });
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
                let node = cap.node;
                let target = node_text(node, content).to_string();
                if target.is_empty() {
                    continue;
                }
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw: target,
                    ref_kind: SymbolRefKind::Call,
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
--------------------------- MODULE Counter ---------------------------
EXTENDS Naturals, TLC

CONSTANTS MaxVal
VARIABLES counter

Init == counter = 0
Next == /\ counter < MaxVal
        /\ counter' = counter + 1

Spec == Init /\ [][Next]_counter

THEOREM Safety == Spec => [](counter <= MaxVal)

=============================================================================
"#;

    #[test]
    fn tlaplus_language_name() {
        assert_eq!(TLAPLUS_BACKEND.language_name(), "tlaplus");
    }

    #[test]
    fn extract_symbols_finds_module_and_operators() {
        let syms = TLAPLUS_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Counter"), "missing module: {:?}", names);
        assert!(names.contains(&"Init"), "missing Init: {:?}", names);
        assert!(names.contains(&"Next"));
        assert!(names.contains(&"Spec"));
    }

    #[test]
    fn extract_symbols_finds_theorem() {
        let syms = TLAPLUS_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Safety"), "missing theorem: {:?}", names);
    }

    #[test]
    fn extract_symbols_finds_constants_and_variables() {
        let syms = TLAPLUS_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MaxVal"));
        assert!(names.contains(&"counter"));
    }

    #[test]
    fn extract_imports_handles_extends() {
        let imports = TLAPLUS_BACKEND.extract_imports(SAMPLE);
        let targets: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(
            targets.contains(&"Naturals"),
            "missing Naturals: {:?}",
            targets
        );
        assert!(targets.contains(&"TLC"));
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        let bogus = "this is not valid TLA+ {{{ syntax";
        let _ = TLAPLUS_BACKEND.extract_symbols(bogus);
        let _ = TLAPLUS_BACKEND.extract_imports(bogus);
        let _ = TLAPLUS_BACKEND.extract_references(bogus);
    }
}
