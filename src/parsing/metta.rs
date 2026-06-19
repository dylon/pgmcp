//! MeTTa language backend (Tier 0e). Uses the user's
//! `tree-sitter-metta` grammar via path dependency in `Cargo.toml`.
//!
//! MeTTa is a hyperedge-graph / term-rewriting language underlying Hyperon
//! and the MeTTaTron Rholang compiler. The tree-sitter grammar is generic
//! S-expressions — semantics come from the **head atom** of each `list`. The
//! backend recognizes:
//!
//! - `(= (head $args…) body)` — rule definition (head identifier → `Function`)
//! - `(:= name body)` — alternate rule binding (name identifier → `Function`)
//! - `(: name TypeExpr)` — type annotation (name identifier → `Trait`, with
//!   TypeExpr identifiers emitted as `TypeUse` references)
//! - `(import! &space file)` — module import (file → `Import`, space → alias)
//! - `!(expr)` at top level — execution prefix; head identifier of inner list
//!   is emitted as a `Call` reference
//! - Generic list with identifier head — `Call` reference to head, filtered
//!   by a deny-list of language-builtin heads
//!
//! Variable-bound forms (`(= $x …)`) are intentionally skipped — they bind,
//! they do not define. Per-function complexity metrics are deferred: MeTTa
//! has no first-class branching constructs, and rule arity (number of `=`-rules
//! sharing a head) is a global property that does not fit the per-function
//! schema cleanly.
//!
//! See: `/home/dylon/Workspace/f1r3fly.io/MeTTa-Compiler/tree-sitter-metta/`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "metta/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static METTA_BACKEND: MettaBackend = MettaBackend;
pub struct MettaBackend;

fn metta_language() -> Language {
    tree_sitter_metta::language()
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&metta_language())
            .expect("set_language metta");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

/// Captures rule definitions (`(= LHS RHS)`), alt rule bindings
/// (`(:= LHS RHS)`), and type annotations (`(: NAME TYPE)`).
const SYMBOL_QUERY: &str = r#"
((list
   . (expression (atom_expression (operator (assignment_operator)))) @op
   . (expression) @sym.lhs
   . (expression) @sym.rhs) @sym.def
 (#eq? @op "="))

((list
   . (expression (atom_expression (operator (rule_definition_operator)))) @op
   . (expression) @sym.lhs
   . (expression) @sym.rhs) @sym.def
 (#eq? @op ":="))

((list
   . (expression (atom_expression (operator (type_annotation_operator)))) @op
   . (expression) @sym.lhs
   . (expression) @sym.rhs) @sym.def
 (#eq? @op ":"))
"#;

/// Captures `(import! …)` lists. We accept import! at any nesting depth so
/// `!(import! &self file)` (the typical top-level form) and bare
/// `(import! …)` both match.
const IMPORT_QUERY: &str = r#"
((list . (expression (atom_expression (identifier) @head))) @import.outer
 (#eq? @head "import!"))
"#;

/// Captures call-shaped references: a list whose first child is an identifier
/// (filtered by `REF_DENY` in Rust), and top-level `!(…)` execution prefixes
/// whose argument list has an identifier head.
const REF_QUERY: &str = r#"
((list . (expression (atom_expression (identifier) @ref.callee))) @ref.call)

((prefixed_expression
   (exclaim_prefix)
   (expression
     (list . (expression (atom_expression (identifier) @exec.callee))))) @exec.toplevel)
"#;

/// Heads that are language built-ins or pattern-binders — not user-callable
/// symbols. Filtering them out of references keeps the call graph clean.
const REF_DENY: &[&str] = &[
    "import!",
    "let",
    "let*",
    "case",
    "if",
    "match",
    "do",
    "collapse",
    "superpose",
    "=",
    ":",
    ":=",
    "->",
    "<-",
    "<<-",
    "quote",
    "unquote",
    "lambda",
    "function",
    "trace!",
    "println!",
    "pragma!",
    "bind!",
    "new-space",
    "add-atom",
    "remove-atom",
    "get-atoms",
];

fn symbol_query() -> &'static Query {
    SYMBOL_Q
        .get_or_init(|| Query::new(&metta_language(), SYMBOL_QUERY).expect("symbol query metta"))
}
fn import_query() -> &'static Query {
    IMPORT_Q
        .get_or_init(|| Query::new(&metta_language(), IMPORT_QUERY).expect("import query metta"))
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| Query::new(&metta_language(), REF_QUERY).expect("ref query metta"))
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
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Strip surrounding `"` quotes from a string literal's text.
fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Strip leading `&` from a `space_reference` token (`&self` → `self`).
fn strip_space_prefix(s: &str) -> &str {
    s.trim().trim_start_matches('&')
}

/// Unwrap a MeTTa `expression` wrapper to its inner content node
/// (`list` | `prefixed_expression` | `atom_expression`). Returns the inner
/// node, or the input itself if it's not an `expression`.
fn unwrap_expression(node: Node<'_>) -> Node<'_> {
    if node.kind() == "expression"
        && let Some(inner) = node.named_child(0)
    {
        return inner;
    }
    node
}

/// Unwrap a MeTTa `atom_expression` wrapper to its inner semantic node
/// (`identifier`, `variable`, `operator`, `string_literal`, etc.).
fn unwrap_atom(node: Node<'_>) -> Node<'_> {
    if node.kind() == "atom_expression"
        && let Some(inner) = node.named_child(0)
    {
        return inner;
    }
    node
}

/// Extract a symbol name from a MeTTa expression. Handles three LHS shapes:
/// - `(foo $x $y)` — list with identifier head → "foo"
/// - `bar` — bare identifier → "bar"
/// - `$x` — variable → returns None (skip; binding, not definition)
/// - anything else (literal, operator, etc.) → None
fn extract_lhs_name(lhs_expr: Node<'_>, src: &str) -> Option<String> {
    let inner = unwrap_expression(lhs_expr);
    match inner.kind() {
        "list" => first_identifier_in_list(inner, src),
        "atom_expression" => {
            let atom = unwrap_atom(inner);
            match atom.kind() {
                "identifier" => Some(node_text(atom, src).to_string()),
                _ => None,
            }
        }
        "identifier" => Some(node_text(inner, src).to_string()),
        _ => None,
    }
}

/// Walk a `list` node's named children and return the first identifier text
/// encountered (descending through `expression` / `atom_expression` wrappers).
fn first_identifier_in_list(list_node: Node<'_>, src: &str) -> Option<String> {
    let mut cursor = list_node.walk();
    for child in list_node.named_children(&mut cursor) {
        let inner = unwrap_expression(child);
        let atom = unwrap_atom(inner);
        if atom.kind() == "identifier" {
            return Some(node_text(atom, src).to_string());
        }
    }
    None
}

/// Walk an arbitrary MeTTa expression subtree and yield every `identifier`
/// text encountered (useful for emitting `TypeUse` references over a type
/// annotation's RHS).
fn collect_identifiers(node: Node<'_>, src: &str, out: &mut Vec<(String, u32)>) {
    if node.kind() == "identifier" {
        let text = node_text(node, src).to_string();
        if !text.is_empty() {
            out.push((text, line_of(node)));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifiers(child, src, out);
    }
}

/// Walk an `import!` list's children and extract `(target_raw, alias)` pairs.
/// Recognized argument shapes:
/// - `&space` → contributes the alias (stripped of `&`).
/// - `identifier` / `string_literal` → contributes the target.
///
/// Multiple identifiers are joined to handle path-like names that the lexer
/// may split on punctuation.
fn extract_import_args(list_node: Node<'_>, src: &str) -> Option<(String, Option<String>)> {
    let mut alias: Option<String> = None;
    let mut target_parts: Vec<String> = Vec::with_capacity(4);
    let mut cursor = list_node.walk();
    let mut saw_head = false;
    for child in list_node.named_children(&mut cursor) {
        let inner = unwrap_expression(child);
        let atom = if inner.kind() == "atom_expression" {
            unwrap_atom(inner)
        } else {
            inner
        };
        let kind = atom.kind();
        if !saw_head {
            // Skip the `import!` head identifier.
            saw_head = true;
            continue;
        }
        match kind {
            "space_reference" => {
                let text = strip_space_prefix(node_text(atom, src)).to_string();
                if !text.is_empty() {
                    alias = Some(text);
                }
            }
            "string_literal" => {
                target_parts.push(strip_quotes(node_text(atom, src)).to_string());
            }
            "identifier" => {
                target_parts.push(node_text(atom, src).to_string());
            }
            _ => {
                // Operators between path segments (e.g. `./util.metta` may
                // tokenize as `.` `/` `util.metta`) contribute their literal
                // text so we can reassemble the path.
                let text = node_text(atom, src).trim().to_string();
                if !text.is_empty() && text != "(" && text != ")" {
                    target_parts.push(text);
                }
            }
        }
    }
    if target_parts.is_empty() {
        return None;
    }
    Some((target_parts.join(""), alias))
}

impl LanguageBackend for MettaBackend {
    fn language_name(&self) -> &'static str {
        "metta"
    }

    fn lex_config(&self) -> crate::parsing::occurrences::LexConfig {
        crate::parsing::occurrences::LexConfig::lisp_style()
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
            let op_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "op");
            let lhs_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "sym.lhs");
            let def_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "sym.def");
            let rhs_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "sym.rhs");
            let (Some(op), Some(lhs), Some(def)) = (op_cap, lhs_cap, def_cap) else {
                continue;
            };
            let op_text = node_text(op.node, content);
            let Some(name) = extract_lhs_name(lhs.node, content) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            // `:` → type annotation; `=`/`:=` → rule/binding.
            let kind = if op_text == ":" {
                SymbolKind::Trait
            } else {
                SymbolKind::Function
            };
            // Signature: include the operator + LHS + RHS first-line.
            let signature_node = def.node;
            let signature = Some(first_line(content, signature_node));

            // Shadow-ASR extraction. For `:` annotations the RHS is a type
            // expression; for `=` / `:=` rules the RHS is a body and the LHS
            // is a pattern from which we derive parameters. `metta_typed`
            // is appended to the return_type_tags of `:`-annotated symbols
            // so downstream tools can recognize them as carrying explicit
            // source type information.
            let (parameters, return_type, effects) = if op_text == ":" {
                if let Some(rhs) = rhs_cap {
                    let mut rt = type_mapper::return_type_from_annotation(rhs.node, content);
                    rt.type_tags
                        .push(crate::parsing::type_tags::vocabulary::TAG_METTA_TYPED.to_string());
                    rt.type_tags.sort();
                    rt.type_tags.dedup();
                    (Vec::new(), Some(rt), Vec::new())
                } else {
                    (Vec::new(), None, Vec::new())
                }
            } else {
                // = or := rule definition.
                let params = type_mapper::parameters_from_rule_lhs(lhs.node, content, None);
                let effs = if let Some(rhs) = rhs_cap {
                    type_mapper::effects_for_rule(op_text, lhs.node, rhs.node, content)
                } else {
                    type_mapper::effects_for_rule(
                        op_text, lhs.node, lhs.node, // fallback to lhs when rhs missing
                        content,
                    )
                };
                (params, None, effs)
            };

            out.push(Symbol {
                file_id: 0,
                kind,
                start_line: line_of(def.node),
                end_line: end_line_of(def.node),
                parent_id: None,
                visibility: None,
                signature,
                name,
                parameters,
                return_type,
                effects,
                scope_path: None,
                scope_depth: Some(0),
                ..Default::default()
            });
            // For type annotations, also emit `TypeUse` references over the
            // RHS so type→type edges show up in the call graph. Handled in
            // `extract_references`; we do not duplicate here.
            let _ = rhs_cap;
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
            let outer_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.outer");
            let Some(outer) = outer_cap else {
                continue;
            };
            let Some((target_raw, alias)) = extract_import_args(outer.node, content) else {
                continue;
            };
            if target_raw.is_empty() {
                continue;
            }
            out.push(Import {
                target_raw,
                source_line: line_of(outer.node),
                alias,
            });
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let mut out: Vec<SymbolReference> = Vec::new();

        // Call-shaped refs from list-head identifiers + top-level `!(…)`
        // execution prefixes.
        let qref = ref_query();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(qref, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = qref.capture_names()[cap.index as usize];
                if cap_name != "ref.callee" && cap_name != "exec.callee" {
                    continue;
                }
                let text = node_text(cap.node, content).to_string();
                if text.is_empty() || REF_DENY.contains(&text.as_str()) {
                    continue;
                }
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw: text,
                    ref_kind: SymbolRefKind::Call,
                    source_line: line_of(cap.node),
                });
            }
        }

        // TypeUse references from `(: NAME TypeExpr)` RHS — walk the symbol
        // query a second time scoped to type annotations.
        let qsym = symbol_query();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(qsym, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            let op_cap = m
                .captures
                .iter()
                .find(|c| qsym.capture_names()[c.index as usize] == "op");
            let rhs_cap = m
                .captures
                .iter()
                .find(|c| qsym.capture_names()[c.index as usize] == "sym.rhs");
            let (Some(op), Some(rhs)) = (op_cap, rhs_cap) else {
                continue;
            };
            if node_text(op.node, content) != ":" {
                continue;
            }
            let mut idents: Vec<(String, u32)> = Vec::new();
            collect_identifiers(rhs.node, content, &mut idents);
            for (text, line) in idents {
                if REF_DENY.contains(&text.as_str()) {
                    continue;
                }
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw: text,
                    ref_kind: SymbolRefKind::TypeUse,
                    source_line: line,
                });
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_loads() {
        let lang = metta_language();
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn language_name_is_metta() {
        assert_eq!(METTA_BACKEND.language_name(), "metta");
    }

    #[test]
    fn extract_symbols_finds_rule_def() {
        let src = "(= (add $x $y) (+ $x $y))\n";
        let syms = METTA_BACKEND.extract_symbols(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"), "names: {:?}", names);
        let add = syms.iter().find(|s| s.name == "add").expect("add symbol");
        assert_eq!(add.kind, SymbolKind::Function);
    }

    #[test]
    fn extract_symbols_finds_constant_rule() {
        let src = "(= pi 3.14159)\n";
        let syms = METTA_BACKEND.extract_symbols(src);
        assert!(
            syms.iter()
                .any(|s| s.name == "pi" && s.kind == SymbolKind::Function),
            "syms: {:?}",
            syms
        );
    }

    #[test]
    fn extract_symbols_finds_type_annotation() {
        let src = "(: Nat Type)\n(: add (-> Number Number Number))\n";
        let syms = METTA_BACKEND.extract_symbols(src);
        let nat = syms.iter().find(|s| s.name == "Nat");
        let add = syms.iter().find(|s| s.name == "add");
        assert!(nat.is_some(), "syms: {:?}", syms);
        assert!(add.is_some(), "syms: {:?}", syms);
        assert_eq!(nat.expect("Nat").kind, SymbolKind::Trait);
        assert_eq!(add.expect("add").kind, SymbolKind::Trait);
    }

    #[test]
    fn extract_symbols_finds_alt_rule_binding() {
        let src = "(:= foo 42)\n";
        let syms = METTA_BACKEND.extract_symbols(src);
        assert!(
            syms.iter()
                .any(|s| s.name == "foo" && s.kind == SymbolKind::Function),
            "syms: {:?}",
            syms
        );
    }

    #[test]
    fn extract_symbols_skips_variable_lhs() {
        let src = "(= $x 1)\n";
        let syms = METTA_BACKEND.extract_symbols(src);
        assert!(
            syms.iter().all(|s| !s.name.starts_with('$')),
            "syms: {:?}",
            syms
        );
    }

    #[test]
    fn extract_imports_handles_space_and_file() {
        let src = "!(import! &self stdlib)\n";
        let imps = METTA_BACKEND.extract_imports(src);
        assert!(
            imps.iter()
                .any(|i| i.target_raw == "stdlib" && i.alias.as_deref() == Some("self")),
            "imports: {:?}",
            imps
        );
    }

    #[test]
    fn extract_imports_handles_string_literal() {
        let src = r#"!(import! &kb "math.metta")"#;
        let imps = METTA_BACKEND.extract_imports(src);
        assert!(
            imps.iter()
                .any(|i| i.target_raw == "math.metta" && i.alias.as_deref() == Some("kb")),
            "imports: {:?}",
            imps
        );
    }

    #[test]
    fn extract_references_finds_top_level_exec() {
        let src = "!(add 1 2)\n";
        let refs = METTA_BACKEND.extract_references(src);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(calls.contains(&"add"), "refs: {:?}", refs);
    }

    #[test]
    fn extract_references_finds_list_head_calls() {
        let src = "(= (greet $x) (println! (foo $x)))\n";
        let refs = METTA_BACKEND.extract_references(src);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(calls.contains(&"foo"), "refs: {:?}", refs);
        // `println!` is in the deny-list — should not appear.
        assert!(!calls.contains(&"println!"), "refs: {:?}", refs);
    }

    #[test]
    fn extract_references_skips_builtin_heads() {
        let src = "(let* ($x 1) (if (== $x 1) 'yes 'no))\n";
        let refs = METTA_BACKEND.extract_references(src);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        for builtin in &["let*", "if", "case", "match", "do"] {
            assert!(
                !calls.contains(builtin),
                "deny-listed head {:?} should not appear; refs: {:?}",
                builtin,
                refs
            );
        }
    }

    #[test]
    fn extract_references_emits_typeuse_for_annotations() {
        let src = "(: add (-> Number Number Number))\n";
        let refs = METTA_BACKEND.extract_references(src);
        let type_uses: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::TypeUse)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(type_uses.contains(&"Number"), "type_uses: {:?}", type_uses);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "(", "(=", "(= )", "(import!"] {
            let _ = METTA_BACKEND.extract_symbols(s);
            let _ = METTA_BACKEND.extract_imports(s);
            let _ = METTA_BACKEND.extract_references(s);
        }
    }
}
