//! Lean 4 language backend using `tree-sitter-lean4`.
//!
//! Symbol queries cover definition (def/theorem/lemma/abbrev) /
//! inductive / structure / opaque / axiom / class_inductive / namespace.
//! Import queries handle `import Foo.Bar` statements. The grammar's `name`
//! field is a multi-identifier sequence (dotted path); we join the parts
//! with dots and return one Symbol per outer-node match.

#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolReference};

pub static LEAN_BACKEND: LeanBackend = LeanBackend;
pub struct LeanBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_lean4::language())
            .expect("set_language lean4");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(definition) @def.node
(inductive) @ind.node
(structure) @struct.node
(opaque) @opaque.node
(axiom) @axiom.node
(class_inductive) @class.node
(namespace) @ns.node
"#;

const IMPORT_QUERY: &str = r#"
(import) @import.node
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_lean4::language(), SYMBOL_QUERY).expect("symbol query lean4")
    })
}

fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_lean4::language(), IMPORT_QUERY).expect("import query lean4")
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

/// Walk children of `node` looking at the given field name, collecting
/// `identifier` / `escaped_identifier` text. Joins with `.` to form a
/// dotted path. Returns empty when the field has no identifier children.
fn dotted_field(node: Node<'_>, field: &str, src: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some(field) {
                let n = cursor.node();
                let k = n.kind();
                if k == "identifier" || k == "escaped_identifier" {
                    parts.push(node_text(n, src).to_string());
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    parts.join(".")
}

fn kind_for_capture(cap: &str) -> Option<SymbolKind> {
    match cap {
        "def.node" => Some(SymbolKind::Function),
        "ind.node" => Some(SymbolKind::Enum),
        "struct.node" => Some(SymbolKind::Struct),
        "opaque.node" | "axiom.node" => Some(SymbolKind::Const),
        "class.node" => Some(SymbolKind::Class),
        "ns.node" => Some(SymbolKind::Module),
        _ => None,
    }
}

impl LanguageBackend for LeanBackend {
    fn language_name(&self) -> &'static str {
        "lean"
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
                let Some(kind) = kind_for_capture(cap_name) else {
                    continue;
                };
                let node = cap.node;
                let name = dotted_field(node, "name", content);
                if name.is_empty() {
                    continue;
                }
                out.push(Symbol {
                    file_id: 0,
                    kind,
                    start_line: line_of(node),
                    end_line: end_line_of(node),
                    parent_id: None,
                    visibility: Some("public".into()),
                    signature: None,
                    name,
                });
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
                let target = dotted_field(node, "module", content);
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

    fn extract_references(&self, _content: &str) -> Vec<SymbolReference> {
        // Lean tactic references (e.g. `apply add_comm`) live inside proof
        // terms; tree-sitter-lean4 0.3 does not expose tactic identifiers as
        // a uniform query target. Empty is the contract-correct response.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
import Mathlib.Data.Nat.Basic
import Mathlib.Tactic.Linarith

namespace MyMath

def double (n : Nat) : Nat := n + n

theorem double_eq_two_mul (n : Nat) : double n = 2 * n := by
  unfold double
  linarith

structure Point where
  x : Nat
  y : Nat

inductive Tree where
  | leaf
  | node : Tree → Tree → Tree

end MyMath
"#;

    #[test]
    fn lean_language_name() {
        assert_eq!(LEAN_BACKEND.language_name(), "lean");
    }

    #[test]
    fn extract_symbols_returns_nonempty_for_sample() {
        let syms = LEAN_BACKEND.extract_symbols(SAMPLE);
        assert!(
            !syms.is_empty(),
            "expected at least one symbol; got {:?}",
            syms.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn extract_imports_finds_mathlib_paths() {
        let imports = LEAN_BACKEND.extract_imports(SAMPLE);
        let targets: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(
            targets.iter().any(|t| t.contains("Mathlib")),
            "expected an import containing 'Mathlib', got {:?}",
            targets
        );
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        let bogus = "this is not valid Lean ¬¬¬ syntax";
        let _ = LEAN_BACKEND.extract_symbols(bogus);
        let _ = LEAN_BACKEND.extract_imports(bogus);
    }
}
