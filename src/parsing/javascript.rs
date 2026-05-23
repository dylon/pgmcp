//! JavaScript + TypeScript language backend (Tier 0e).
//!
//! One module covers three registry entries:
//! - `JS_BACKEND`  — `language_name() = "javascript"`, parses with `tree-sitter-javascript`.
//! - `TS_BACKEND`  — `language_name() = "typescript"`, parses with `tree-sitter-typescript::LANGUAGE_TYPESCRIPT`.
//! - `TSX_BACKEND` — `language_name() = "tsx"`, parses with `LANGUAGE_TSX` (handles JSX).
//!
//! Today, pgmcp's `indexed_files.language` only emits `"javascript"` and
//! `"typescript"`. The TSX backend exists for a future config update that
//! splits `.tsx` into its own language label.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "javascript/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

#[derive(Clone, Copy)]
enum Variant {
    Js,
    Ts,
    Tsx,
}

pub struct JsTsBackend {
    variant: Variant,
}

pub static JS_BACKEND: JsTsBackend = JsTsBackend {
    variant: Variant::Js,
};
pub static TS_BACKEND: JsTsBackend = JsTsBackend {
    variant: Variant::Ts,
};
pub static TSX_BACKEND: JsTsBackend = JsTsBackend {
    variant: Variant::Tsx,
};

fn language_for(variant: Variant) -> Language {
    match variant {
        Variant::Js => tree_sitter_javascript::LANGUAGE.into(),
        Variant::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Variant::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

thread_local! {
    static JS_PARSER: RefCell<Parser> = RefCell::new(make_parser(Variant::Js));
    static TS_PARSER: RefCell<Parser> = RefCell::new(make_parser(Variant::Ts));
    static TSX_PARSER: RefCell<Parser> = RefCell::new(make_parser(Variant::Tsx));
}

fn make_parser(variant: Variant) -> Parser {
    let mut p = Parser::new();
    p.set_language(&language_for(variant))
        .expect("set_language js/ts");
    p
}

fn parse(content: &str, variant: Variant) -> Option<Tree> {
    let f = |p: &RefCell<Parser>| p.borrow_mut().parse(content, None);
    match variant {
        Variant::Js => JS_PARSER.with(f),
        Variant::Ts => TS_PARSER.with(f),
        Variant::Tsx => TSX_PARSER.with(f),
    }
}

// ============================================================================
// Queries — compiled once per (variant, kind).
// ============================================================================

const SYMBOL_QUERY_JS: &str = r#"
(function_declaration name: (identifier) @sym.func.name) @sym.func
(class_declaration name: (identifier) @sym.class.name) @sym.class
(class_body
  (method_definition
    name: [(property_identifier) (private_property_identifier)] @sym.method.name) @sym.method)
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @sym.const.name)) @sym.const)
(program
  (variable_declaration
    (variable_declarator name: (identifier) @sym.const.name)) @sym.const)
"#;

const SYMBOL_QUERY_TS: &str = r#"
(function_declaration name: (identifier) @sym.func.name) @sym.func
(class_declaration name: (type_identifier) @sym.class.name) @sym.class
(class_body
  (method_definition
    name: [(property_identifier) (private_property_identifier)] @sym.method.name) @sym.method)
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @sym.const.name)) @sym.const)
(program
  (variable_declaration
    (variable_declarator name: (identifier) @sym.const.name)) @sym.const)
(interface_declaration name: (type_identifier) @sym.iface.name) @sym.iface
(type_alias_declaration name: (type_identifier) @sym.alias.name) @sym.alias
(enum_declaration name: (identifier) @sym.enum.name) @sym.enum
"#;

const IMPORT_QUERY: &str = r#"
(import_statement source: (string) @imp.target) @imp.stmt
(export_statement source: (string) @rexp.target) @rexp.stmt
(call_expression
  function: (identifier) @cjs.fn
  arguments: (arguments (string) @cjs.target)) @cjs.call
"#;

const REF_QUERY_JS: &str = r#"
(call_expression function: (identifier) @ref.call) @ref.call.node
(call_expression
  function: (member_expression
              property: (property_identifier) @ref.mcall)) @ref.mcall.node
(class_heritage (identifier) @ref.inherit)
"#;

const REF_QUERY_TS: &str = r#"
(call_expression function: (identifier) @ref.call) @ref.call.node
(call_expression
  function: (member_expression
              property: (property_identifier) @ref.mcall)) @ref.mcall.node
(extends_clause (identifier) @ref.inherit)
(implements_clause (type_identifier) @ref.impl)
(type_annotation (type_identifier) @ref.type)
(type_arguments (type_identifier) @ref.type)
"#;

const REF_QUERY_TSX_EXT: &str = r#"
(jsx_self_closing_element name: (identifier) @ref.jsx)
(jsx_opening_element name: (identifier) @ref.jsx)
"#;

static SYMBOL_Q_JS: OnceLock<Query> = OnceLock::new();
static SYMBOL_Q_TS: OnceLock<Query> = OnceLock::new();
static SYMBOL_Q_TSX: OnceLock<Query> = OnceLock::new();
static IMPORT_Q_JS: OnceLock<Query> = OnceLock::new();
static IMPORT_Q_TS: OnceLock<Query> = OnceLock::new();
static IMPORT_Q_TSX: OnceLock<Query> = OnceLock::new();
static REF_Q_JS: OnceLock<Query> = OnceLock::new();
static REF_Q_TS: OnceLock<Query> = OnceLock::new();
static REF_Q_TSX: OnceLock<Query> = OnceLock::new();

fn query_for(variant: Variant, kind: QueryKind) -> &'static Query {
    let lang = language_for(variant);
    match (variant, kind) {
        (Variant::Js, QueryKind::Symbol) => {
            SYMBOL_Q_JS.get_or_init(|| Query::new(&lang, SYMBOL_QUERY_JS).expect("symbol query js"))
        }
        (Variant::Ts, QueryKind::Symbol) => {
            SYMBOL_Q_TS.get_or_init(|| Query::new(&lang, SYMBOL_QUERY_TS).expect("symbol query ts"))
        }
        (Variant::Tsx, QueryKind::Symbol) => SYMBOL_Q_TSX
            .get_or_init(|| Query::new(&lang, SYMBOL_QUERY_TS).expect("symbol query tsx")),
        (Variant::Js, QueryKind::Import) => {
            IMPORT_Q_JS.get_or_init(|| Query::new(&lang, IMPORT_QUERY).expect("import query js"))
        }
        (Variant::Ts, QueryKind::Import) => {
            IMPORT_Q_TS.get_or_init(|| Query::new(&lang, IMPORT_QUERY).expect("import query ts"))
        }
        (Variant::Tsx, QueryKind::Import) => {
            IMPORT_Q_TSX.get_or_init(|| Query::new(&lang, IMPORT_QUERY).expect("import query tsx"))
        }
        (Variant::Js, QueryKind::Reference) => {
            REF_Q_JS.get_or_init(|| Query::new(&lang, REF_QUERY_JS).expect("ref query js"))
        }
        (Variant::Ts, QueryKind::Reference) => {
            REF_Q_TS.get_or_init(|| Query::new(&lang, REF_QUERY_TS).expect("ref query ts"))
        }
        (Variant::Tsx, QueryKind::Reference) => REF_Q_TSX.get_or_init(|| {
            let combined = format!("{}\n{}", REF_QUERY_TS, REF_QUERY_TSX_EXT);
            Query::new(&lang, &combined).expect("ref query tsx")
        }),
    }
}

#[derive(Clone, Copy)]
enum QueryKind {
    Symbol,
    Import,
    Reference,
}

// ============================================================================
// Helpers
// ============================================================================

fn line_of(node: Node<'_>) -> u32 {
    (node.start_position().row as u32) + 1
}

fn end_line_of(node: Node<'_>) -> u32 {
    (node.end_position().row as u32) + 1
}

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('`') && s.ends_with('`'))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
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

fn first_capitalized(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

// ============================================================================
// LanguageBackend impl
// ============================================================================

impl LanguageBackend for JsTsBackend {
    fn language_name(&self) -> &'static str {
        match self.variant {
            Variant::Js => "javascript",
            Variant::Ts => "typescript",
            Variant::Tsx => "tsx",
        }
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = query_for(self.variant, QueryKind::Symbol);
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Symbol> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                match cap_name {
                    "sym.func" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            push_named(node, name_node, content, SymbolKind::Function, &mut out);
                        }
                    }
                    "sym.class" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            push_named(node, name_node, content, SymbolKind::Class, &mut out);
                        }
                    }
                    "sym.method" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(name_node, content).to_string();
                            // Private fields with `#` prefix are private; default public.
                            let visibility = if name.starts_with('#') {
                                Some("private".into())
                            } else {
                                Some("public".into())
                            };
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Function,
                                start_line: line_of(node),
                                end_line: end_line_of(node),
                                parent_id: None,
                                visibility,
                                signature: Some(name.clone()),
                                name,
                                ..Default::default()
                            });
                        }
                    }
                    "sym.const.name" => {
                        let name = node_text(node, content).to_string();
                        if is_screaming_snake(&name) {
                            out.push(Symbol {
                                file_id: 0,
                                kind: SymbolKind::Const,
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
                    "sym.iface" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            push_named(node, name_node, content, SymbolKind::Interface, &mut out);
                        }
                    }
                    "sym.alias" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            push_named(node, name_node, content, SymbolKind::Other, &mut out);
                        }
                    }
                    "sym.enum" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            push_named(node, name_node, content, SymbolKind::Enum, &mut out);
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = query_for(self.variant, QueryKind::Import);
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Import> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            // Per-match dispatch — we want the @imp.stmt / @rexp.stmt /
            // @cjs.call capture so we can walk into the import_clause for aliases.
            let stmt_cap = m.captures.iter().find(|c| {
                let n = q.capture_names()[c.index as usize];
                n == "imp.stmt" || n == "rexp.stmt" || n == "cjs.call"
            });
            let target_cap = m.captures.iter().find(|c| {
                let n = q.capture_names()[c.index as usize];
                n == "imp.target" || n == "rexp.target" || n == "cjs.target"
            });
            let (Some(stmt), Some(target)) = (stmt_cap, target_cap) else {
                continue;
            };
            let stmt_kind = q.capture_names()[stmt.index as usize];
            let stmt_node = stmt.node;
            let target_text = strip_quotes(node_text(target.node, content)).to_string();
            let line = line_of(stmt_node);

            match stmt_kind {
                "imp.stmt" => walk_import_clause(stmt_node, content, &target_text, line, &mut out),
                "rexp.stmt" => out.push(Import {
                    target_raw: target_text,
                    source_line: line,
                    alias: None,
                }),
                "cjs.call" => {
                    // Only emit if the call's function is `require` (filter).
                    let fn_cap = m
                        .captures
                        .iter()
                        .find(|c| q.capture_names()[c.index as usize] == "cjs.fn");
                    if let Some(f) = fn_cap
                        && node_text(f.node, content) == "require"
                    {
                        out.push(Import {
                            target_raw: target_text,
                            source_line: line,
                            alias: None,
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = query_for(self.variant, QueryKind::Reference);
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
                    "ref.call" | "ref.mcall" => SymbolRefKind::Call,
                    "ref.inherit" => SymbolRefKind::Inherit,
                    "ref.impl" => SymbolRefKind::Impl,
                    "ref.type" => SymbolRefKind::TypeUse,
                    "ref.jsx" => {
                        // Skip lower-case JSX (HTML elements like <div />).
                        if !first_capitalized(&target_raw) {
                            continue;
                        }
                        SymbolRefKind::Call
                    }
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

fn push_named(
    node: Node<'_>,
    name_node: Node<'_>,
    src: &str,
    kind: SymbolKind,
    out: &mut Vec<Symbol>,
) {
    let name = node_text(name_node, src).to_string();
    // Shadow-ASR: when this is a function-shaped node, pull parameters /
    // return type / generics / effects via the type_mapper. For
    // class/interface/enum/alias nodes the function-only fields stay empty.
    let (parameters, return_type, generic_params, effects) = if matches!(
        node.kind(),
        "function_declaration"
            | "function_expression"
            | "arrow_function"
            | "generator_function_declaration"
            | "method_definition"
    ) {
        let params = node
            .child_by_field_name("parameters")
            .map(|p| type_mapper::parameters_from_node(p, src))
            .unwrap_or_default();
        let rt = node.child_by_field_name("return_type");
        let return_type = if rt.is_some() {
            Some(type_mapper::return_type_from_node(rt, src))
        } else {
            None
        };
        let generics = type_mapper::generics_for_function(node, src);
        let effects = type_mapper::effects_for_function(node, src);
        (params, return_type, generics, effects)
    } else {
        (Vec::new(), None, Vec::new(), Vec::new())
    };
    out.push(Symbol {
        file_id: 0,
        kind,
        start_line: line_of(node),
        end_line: end_line_of(node),
        parent_id: None,
        visibility: Some("public".into()),
        signature: Some(name.clone()),
        name,
        parameters,
        return_type,
        generic_params,
        effects,
        scope_depth: Some(0),
        ..Default::default()
    });
}

/// Walk the `import_clause` of an ESM `import_statement`, emitting one row
/// per binding. Side-effect imports (no clause) emit one row with no alias.
fn walk_import_clause(
    stmt: Node<'_>,
    src: &str,
    target_raw: &str,
    line: u32,
    out: &mut Vec<Import>,
) {
    // The grammar exposes children: `import_clause` (optional), `source` (string), separators.
    let mut walker = stmt.walk();
    let mut clause: Option<Node<'_>> = None;
    for child in stmt.children(&mut walker) {
        if child.kind() == "import_clause" {
            clause = Some(child);
            break;
        }
    }
    let clause = match clause {
        Some(c) => c,
        None => {
            // Side-effect import: `import 'mod'`.
            out.push(Import {
                target_raw: target_raw.to_string(),
                source_line: line,
                alias: None,
            });
            return;
        }
    };

    let mut emitted_any = false;
    let mut walker = clause.walk();
    for child in clause.named_children(&mut walker) {
        match child.kind() {
            // Default import: `import x from 'mod'`
            "identifier" => {
                let alias = node_text(child, src).to_string();
                out.push(Import {
                    target_raw: target_raw.to_string(),
                    source_line: line,
                    alias: Some(alias),
                });
                emitted_any = true;
            }
            // Named imports: `import { a, b as c } from 'mod'`
            "named_imports" => {
                let mut nw = child.walk();
                for spec in child.named_children(&mut nw) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    // Each import_specifier has either: `name: identifier` (default)
                    // or `name: identifier alias: identifier` (rebinding).
                    let mut name_local: Option<String> = None;
                    let mut sw = spec.walk();
                    let idents: Vec<Node<'_>> = spec
                        .children(&mut sw)
                        .filter(|c| c.kind() == "identifier")
                        .collect();
                    // Last identifier is the local binding (alias if present, else name).
                    if let Some(last) = idents.last() {
                        name_local = Some(node_text(*last, src).to_string());
                    }
                    out.push(Import {
                        target_raw: target_raw.to_string(),
                        source_line: line,
                        alias: name_local,
                    });
                    emitted_any = true;
                }
            }
            // Namespace import: `import * as ns from 'mod'`
            "namespace_import" => {
                let mut nsw = child.walk();
                for c in child.named_children(&mut nsw) {
                    if c.kind() == "identifier" {
                        out.push(Import {
                            target_raw: target_raw.to_string(),
                            source_line: line,
                            alias: Some(node_text(c, src).to_string()),
                        });
                        emitted_any = true;
                    }
                }
            }
            _ => {}
        }
    }

    if !emitted_any {
        // Defensive: empty clause.
        out.push(Import {
            target_raw: target_raw.to_string(),
            source_line: line,
            alias: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS_SAMPLE: &str = r#"
import { foo, bar as b } from './lib';
import type { Config } from './types';
import * as utils from './utils';

export interface Greeter {
    greet(name: string): string;
}

export class HelloGreeter implements Greeter {
    constructor(private prefix: string) {}
    greet(name: string): string {
        return foo(this.prefix + name);
    }
}
"#;

    #[test]
    fn ts_extract_symbols_finds_class_interface_methods() {
        let syms = TS_BACKEND.extract_symbols(TS_SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "names: {:?}", names);
        assert!(names.contains(&"HelloGreeter"));
        assert!(names.contains(&"greet"));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Interface));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Class));
    }

    #[test]
    fn ts_extract_imports_handles_named_default_namespace() {
        let imps = TS_BACKEND.extract_imports(TS_SAMPLE);
        // Three import statements; named expansion gives 4 rows total (foo, b, Config, utils).
        assert_eq!(imps.len(), 4, "imports: {:?}", imps);
        let pairs: Vec<(&str, Option<&str>)> = imps
            .iter()
            .map(|i| (i.target_raw.as_str(), i.alias.as_deref()))
            .collect();
        assert!(pairs.contains(&("./lib", Some("foo"))));
        assert!(pairs.contains(&("./lib", Some("b"))));
        assert!(pairs.contains(&("./types", Some("Config"))));
        assert!(pairs.contains(&("./utils", Some("utils"))));
    }

    #[test]
    fn ts_extract_references_finds_call_and_implements() {
        let refs = TS_BACKEND.extract_references(TS_SAMPLE);
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Call)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(calls.contains(&"foo"), "calls: {:?}", calls);
        let impls: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Impl)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(impls.contains(&"Greeter"), "impls: {:?}", impls);
    }

    #[test]
    fn js_handles_commonjs_require() {
        let src = "const x = require('./mod');\nconst y = require(varname);\n";
        let imps = JS_BACKEND.extract_imports(src);
        // Only static-string require should match.
        let targets: Vec<&str> = imps.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(targets.contains(&"./mod"));
        // `require(varname)` does not match the (string) capture.
    }

    #[test]
    fn jsx_filters_lowercase() {
        let src = "function App() { return <Foo><div /><Bar /></Foo>; }";
        let refs = TSX_BACKEND.extract_references(src);
        let jsx: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Call)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(jsx.contains(&"Foo"));
        assert!(jsx.contains(&"Bar"));
        assert!(
            !jsx.contains(&"div"),
            "lowercase jsx must not be captured: {:?}",
            jsx
        );
    }

    #[test]
    fn js_extract_references_finds_inherit_and_call() {
        let src = "class A extends B { f() { g(); } }";
        let refs = JS_BACKEND.extract_references(src);
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Call)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(calls.contains(&"g"), "calls: {:?}", calls);
        let inherits: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Inherit)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(inherits.contains(&"B"), "inherits: {:?}", inherits);
    }

    // Forces every (backend, extract_*) pair through OnceLock::get_or_init so
    // a malformed query string fails CI instead of crashing a worker in prod
    // (the JS-Reference path was the only uncovered combination and shipped
    // a NodeType("extends_clause") panic — see plan snoopy-kahn).
    #[test]
    fn all_backends_compile_queries_and_handle_garbage() {
        let backends: [&JsTsBackend; 3] = [&JS_BACKEND, &TS_BACKEND, &TSX_BACKEND];
        for s in ["", "   ", "import {", "function (", "class A extends B {}"] {
            for b in backends {
                let _ = b.extract_symbols(s);
                let _ = b.extract_imports(s);
                let _ = b.extract_references(s);
            }
        }
    }

    #[test]
    fn language_names_match_variants() {
        assert_eq!(JS_BACKEND.language_name(), "javascript");
        assert_eq!(TS_BACKEND.language_name(), "typescript");
        assert_eq!(TSX_BACKEND.language_name(), "tsx");
    }
}
