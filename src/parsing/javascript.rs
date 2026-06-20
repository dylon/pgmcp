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
use crate::parsing::complexity;
use crate::parsing::function_metrics::{
    CognitiveIncrement, CognitiveKind, FunctionMetrics, ScoringInput,
};
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

    fn extract_function_metrics(&self, content: &str) -> Vec<FunctionMetrics> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let mut out: Vec<FunctionMetrics> = Vec::new();
        collect_function_metrics(tree.root_node(), content, &mut out);
        out
    }
}

// ============================================================================
// extract_function_metrics — recursive walker (CC / Cognitive / Halstead /
// NPath / throw-paths). Mirrors `src/parsing/python.rs`'s tree-sitter pass.
// ============================================================================

/// Node kinds that are function bodies / definitions; each gets its own
/// metrics row, and the walker stops at nested ones so they don't pollute the
/// enclosing function's counts.
const JS_FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "arrow_function",
    "generator_function",
    "generator_function_declaration",
    "method_definition",
];

/// Resolve the display name of a function-shaped node. Anonymous functions /
/// arrows fall back to a synthetic name keyed on their start position so they
/// remain distinct rows; the function-metrics cron resolves `function_id` by
/// `(name, start_line)` against `file_symbols`, so anonymous functions simply
/// won't match a symbol row and are skipped at persist time — harmless.
fn function_name(node: Node<'_>, src: &str) -> String {
    if let Some(name_node) = node.child_by_field_name("name") {
        let n = node_text(name_node, src);
        if !n.is_empty() {
            return n.to_string();
        }
    }
    // `const greet = (x) => ...` / `const f = function () {}` — the name lives
    // on the enclosing `variable_declarator`'s `name` field.
    if let Some(parent) = node.parent()
        && parent.kind() == "variable_declarator"
        && let Some(name_node) = parent.child_by_field_name("name")
    {
        let n = node_text(name_node, src);
        if !n.is_empty() {
            return n.to_string();
        }
    }
    // `{ method() {} }` shorthand on an object — the `pair`/property key.
    format!("<anonymous@{}>", line_of(node))
}

/// Walk every function-shaped node in the tree and score it.
fn collect_function_metrics(node: Node<'_>, src: &str, out: &mut Vec<FunctionMetrics>) {
    if JS_FUNCTION_KINDS.contains(&node.kind()) {
        // The function's executable region is its `body` field (a
        // statement_block) or, for a single-expression arrow, its last child.
        let body = node.child_by_field_name("body").unwrap_or_else(|| {
            node.child(node.child_count().saturating_sub(1))
                .unwrap_or(node)
        });
        let name = function_name(node, src);
        out.push(score_js_function(
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

/// Score one JS/TS function body.
fn score_js_function(
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

    walk_js_body(
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
        unsafe_blocks: 0, // not meaningful for JS/TS
    };
    complexity::score(&input)
}

/// Static set of JS/TS operator/keyword tokens (η1 universe). tree-sitter
/// emits punctuation/operators as anonymous leaf nodes whose `kind()` is the
/// literal text, and keywords likewise; we classify both by matching text.
const JS_OPERATOR_KINDS: &[&str] = &[
    // Arithmetic / comparison / logical / bitwise.
    "+",
    "-",
    "*",
    "/",
    "%",
    "**",
    "==",
    "===",
    "!=",
    "!==",
    "<",
    ">",
    "<=",
    ">=",
    "&&",
    "||",
    "??",
    "!",
    "&",
    "|",
    "^",
    "<<",
    ">>",
    ">>>",
    "~", // Assignment.
    "=",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "**=",
    "&&=",
    "||=",
    "??=",
    "&=",
    "|=",
    "^=",
    "<<=",
    ">>=",
    ">>>=", // Member / arrow / spread / optional-chaining.
    ".",
    "?.",
    "...",
    "=>",
    "?",
    ":",
    ",",
    ";",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    // Keywords classified as operators (control-flow + binding + declaration).
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "return",
    "throw",
    "try",
    "catch",
    "finally",
    "function",
    "class",
    "const",
    "let",
    "var",
    "new",
    "delete",
    "typeof",
    "instanceof",
    "in",
    "of",
    "void",
    "yield",
    "await",
    "async",
    "extends",
    "implements",
    "interface",
    "type",
    "enum",
    "import",
    "export",
    "from",
    "as",
];

fn match_js_operator(s: &str) -> Option<&'static str> {
    JS_OPERATOR_KINDS.iter().copied().find(|t| *t == s)
}

#[allow(clippy::too_many_arguments)]
fn walk_js_body(
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
            if let Some(op) = match_js_operator(text) {
                *operators.entry(op).or_insert(0) += 1;
            } else if matches!(
                kind,
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "number"
                    | "string_fragment"
                    | "true"
                    | "false"
                    | "null"
                    | "undefined"
                    | "this"
            ) {
                *operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
    }

    // Decision points & cognitive increments.
    let mut new_depth = depth;
    match kind {
        "if_statement" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            // `else` branch (else_clause / alternative) doubles the paths.
            let has_else = node.child_by_field_name("alternative").is_some();
            npath_factors.push(if has_else { 2 } else { 1 });
            new_depth = depth.saturating_add(1);
        }
        "while_statement" | "for_statement" | "for_in_statement" | "do_statement" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(2);
            new_depth = depth.saturating_add(1);
        }
        "catch_clause" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(2);
        }
        // Each `case` (but not `default`) is a decision point.
        "switch_case" => {
            *decision_points = decision_points.saturating_add(1);
            npath_factors.push(2);
        }
        "ternary_expression" => {
            *decision_points = decision_points.saturating_add(1);
            npath_factors.push(2);
        }
        // Short-circuit boolean / nullish-coalescing operators are decisions.
        "binary_expression" => {
            if let Some(op) = node.child_by_field_name("operator") {
                let op_text = node_text(op, src);
                if matches!(op_text, "&&" | "||" | "??") {
                    *decision_points = decision_points.saturating_add(1);
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::LogicalSequence,
                    });
                    npath_factors.push(2);
                }
            }
        }
        "throw_statement" => {
            *panic_paths = panic_paths.saturating_add(1);
        }
        "break_statement" | "continue_statement" => {
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::BreakInFlow,
            });
        }
        "call_expression" => {
            // Recursion detection: a call whose callee is an identifier equal
            // to the enclosing function name.
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
        // metrics row from `collect_function_metrics`.
        _ if JS_FUNCTION_KINDS.contains(&kind) => {
            return;
        }
        _ => {}
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        walk_js_body(
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

    // ========================================================================
    // extract_function_metrics tests (TS/JS, Group 1c)
    // ========================================================================

    #[test]
    fn ts_cc_for_empty_fn_is_one() {
        let src = "function empty(): void {}";
        let m = TS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1, "metrics: {:?}", m);
        assert_eq!(m[0].name, "empty");
        assert_eq!(m[0].cyclomatic, 1);
    }

    #[test]
    fn js_cc_for_if_else_and_loop() {
        let src = r#"
function classify(x) {
    if (x > 0) {
        for (let i = 0; i < x; i++) {
            doThing(i);
        }
    } else {
        return -1;
    }
    return 0;
}
"#;
        let m = JS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        // 1 if + 1 for = 2 decision points → CC = 3
        assert_eq!(m[0].cyclomatic, 3, "metrics: {:?}", m[0]);
    }

    #[test]
    fn ts_switch_cases_count_as_decisions() {
        let src = r#"
function pick(x: number): string {
    switch (x) {
        case 1: return "a";
        case 2: return "b";
        default: return "z";
    }
}
"#;
        let m = TS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        // 2 `case` (default excluded) → CC = 3
        assert_eq!(m[0].cyclomatic, 3, "metrics: {:?}", m[0]);
    }

    #[test]
    fn js_logical_and_ternary_count_as_decisions() {
        let src = "function f(a, b, c) { return a && b ? c : (b || c); }";
        let m = JS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        // `a && b` (+1), ternary (+1), `b || c` (+1) → CC >= 4
        assert!(m[0].cyclomatic >= 4, "got CC = {}", m[0].cyclomatic);
    }

    #[test]
    fn ts_throw_counts_as_panic_path() {
        let src = r#"
function guard(x: number): number {
    if (x < 0) {
        throw new Error("negative");
    }
    return x;
}
"#;
        let m = TS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].panic_paths, 1, "metrics: {:?}", m[0]);
    }

    #[test]
    fn js_cognitive_increases_with_nesting() {
        let src = r#"
function deep(x) {
    if (x > 0) {
        if (x > 1) {
            return 2;
        }
    }
    return 0;
}
"#;
        let m = JS_BACKEND.extract_function_metrics(src);
        // outer if +1, inner if +2 → cognitive >= 3
        assert!(m[0].cognitive >= 3, "got cognitive = {}", m[0].cognitive);
    }

    #[test]
    fn ts_methods_score_independently() {
        let src = r#"
class S {
    methodA(x: number): number {
        if (x > 0) { return 1; } else { return 0; }
    }
    methodB(): void {}
}
"#;
        let m = TS_BACKEND.extract_function_metrics(src);
        let names: Vec<&str> = m.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"methodA"), "names: {:?}", names);
        assert!(names.contains(&"methodB"));
        let a = m.iter().find(|x| x.name == "methodA").expect("methodA");
        let b = m.iter().find(|x| x.name == "methodB").expect("methodB");
        assert_eq!(a.cyclomatic, 2); // one if
        assert_eq!(b.cyclomatic, 1); // empty
    }

    #[test]
    fn js_arrow_assigned_to_const_gets_name() {
        let src = "const greet = (name) => { if (name) { return name; } return \"hi\"; };";
        let m = JS_BACKEND.extract_function_metrics(src);
        let greet = m.iter().find(|x| x.name == "greet");
        assert!(greet.is_some(), "expected named arrow, got: {:?}", m);
        assert_eq!(greet.expect("greet").cyclomatic, 2); // one if
    }

    #[test]
    fn js_halstead_counts_operators() {
        let src = "function add(a, b) { return a + b; }";
        let m = JS_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        assert!(m[0].halstead.n1 > 0, "metrics: {:?}", m[0]);
        assert!(m[0].halstead.n2 > 0);
    }

    #[test]
    fn ts_parse_error_yields_empty_fn_metrics() {
        let bogus = "function ( { this is not valid";
        // Tree-sitter is error-tolerant, so we just assert no panic and a
        // bounded result (it may extract 0 functions from the error tree).
        let _ = TS_BACKEND.extract_function_metrics(bogus);
    }
}
