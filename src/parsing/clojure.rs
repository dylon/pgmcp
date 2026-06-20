//! Clojure + ClojureScript language backend (Tier 0e). Uses the shared
//! `tree-sitter-clojure` grammar; CLJS-specific quirks (string-module
//! requires, etc.) are handled in the import walker.
//!
//! Shadow-ASR contract: Clojure is dynamically typed; arguments are
//! symbols without static type annotations. Symbols emitted here leave
//! the shadow-ASR fields (`parameters`, `return_type`, `generic_params`,
//! `effects`, `type_tags`) at their `Default::default()` values per the
//! plan (`~/.claude/plans/would-translating-the-asts-cosmic-quill.md`
//! § Phase C). A future revision could mine `^:foo` metadata (e.g.
//! `^:deprecated`, `^:dynamic`, `^:test`) for effect tags without
//! attempting type inference.
//!
//! Strategy: tree-sitter `#eq?` / `#any-of?` predicates dispatch on the
//! head symbol of each `list_lit` form. The deny-list filters special forms
//! out of `extract_references`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::backend::LanguageBackend;
use crate::parsing::complexity;
use crate::parsing::function_metrics::{
    CognitiveIncrement, CognitiveKind, FunctionMetrics, ScoringInput,
};
use crate::parsing::symbols::{
    Import, Parameter, Symbol, SymbolKind, SymbolRefKind, SymbolReference,
};

#[derive(Clone, Copy)]
enum Variant {
    Clj,
    Cljs,
}

pub struct ClojureBackend {
    variant: Variant,
}

pub static CLOJURE_BACKEND: ClojureBackend = ClojureBackend {
    variant: Variant::Clj,
};
pub static CLOJURESCRIPT_BACKEND: ClojureBackend = ClojureBackend {
    variant: Variant::Cljs,
};

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_clojure::LANGUAGE.into())
            .expect("set_language clojure");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
((list_lit . (sym_lit) @head . (sym_lit) @ns.name)
 (#eq? @head "ns")) @ns.def

((list_lit . (sym_lit) @head . (sym_lit) @const.name)
 (#eq? @head "def")) @const.def

((list_lit . (sym_lit) @head . (sym_lit) @fn.name)
 (#any-of? @head "defn" "defn-")) @fn.def

((list_lit . (sym_lit) @head . (sym_lit) @macro.name)
 (#eq? @head "defmacro")) @macro.def

((list_lit . (sym_lit) @head . (sym_lit) @proto.name)
 (#eq? @head "defprotocol")) @proto.def

((list_lit . (sym_lit) @head . (sym_lit) @struct.name)
 (#any-of? @head "defrecord" "deftype")) @struct.def

((list_lit . (sym_lit) @head . (sym_lit) @multi.name)
 (#any-of? @head "defmulti" "defmethod")) @multi.def
"#;

const IMPORT_QUERY: &str = r#"
((list_lit . (sym_lit) @head)
 (#any-of? @head "ns" "require" "use" "import")) @import.outer
"#;

const REF_QUERY: &str = r#"
(list_lit . (sym_lit) @ref.call)
"#;

/// Special-form / def-macro names that should NOT be emitted as references.
const REF_DENY: &[&str] = &[
    ".",
    "..",
    "binding",
    "case",
    "catch",
    "cond",
    "cond->",
    "cond->>",
    "condp",
    "def",
    "defmacro",
    "defmethod",
    "defmulti",
    "defn",
    "defn-",
    "defprotocol",
    "defrecord",
    "deftype",
    "do",
    "do-template",
    "doseq",
    "dotimes",
    "extend-protocol",
    "extend-type",
    "finally",
    "fn",
    "fn*",
    "for",
    "if",
    "if-let",
    "if-not",
    "if-some",
    "import",
    "let",
    "letfn",
    "loop",
    "monitor-enter",
    "monitor-exit",
    "new",
    "ns",
    "quote",
    "recur",
    "reify",
    "require",
    "set!",
    "throw",
    "try",
    "use",
    "var",
    "when",
    "when-first",
    "when-let",
    "when-not",
    "when-some",
];

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_clojure::LANGUAGE.into(), SYMBOL_QUERY)
            .expect("symbol query clojure")
    })
}
fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_clojure::LANGUAGE.into(), IMPORT_QUERY)
            .expect("import query clojure")
    })
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_clojure::LANGUAGE.into(), REF_QUERY).expect("ref query clojure")
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
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Strip surrounding double-quotes from a `str_lit` text.
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

impl LanguageBackend for ClojureBackend {
    fn language_name(&self) -> &'static str {
        match self.variant {
            Variant::Clj => "clojure",
            Variant::Cljs => "clojurescript",
        }
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
            // Each capture set has `@head` + a name capture + an outer `@*.def`.
            // We use the name capture for the symbol name and the def capture
            // for line ranges.
            let head_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "head");
            let Some(head) = head_cap else {
                continue;
            };
            let head_text = node_text(head.node, content).to_string();

            let def_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize].ends_with(".def"));
            let name_cap = m.captures.iter().find(|c| {
                let n = q.capture_names()[c.index as usize];
                n.ends_with(".name")
            });
            let (Some(def), Some(name_node_cap)) = (def_cap, name_cap) else {
                continue;
            };

            let name = node_text(name_node_cap.node, content).to_string();
            if name.is_empty() {
                continue;
            }

            let kind = match head_text.as_str() {
                "ns" => SymbolKind::Module,
                "def" => SymbolKind::Const,
                "defn" | "defn-" | "defmacro" | "defmulti" | "defmethod" => SymbolKind::Function,
                "defprotocol" => SymbolKind::Trait,
                "defrecord" | "deftype" => SymbolKind::Struct,
                _ => continue,
            };

            // Visibility: defn- → private; otherwise public.
            let visibility = if head_text == "defn-" {
                Some("private".into())
            } else {
                Some("public".into())
            };

            // Filter `def` whose value is a function literal — that's
            // effectively a defn with extra ceremony; skip to avoid dual
            // emission. Detect by looking at the third named child of the
            // def's list_lit.
            if matches!(kind, SymbolKind::Const)
                && let Some(third) = def.node.named_child(2)
                && third.kind() == "list_lit"
                && let Some(inner_head) = third.named_child(0)
                && inner_head.kind() == "sym_lit"
            {
                let head_inner = node_text(inner_head, content);
                if matches!(head_inner, "fn" | "fn*" | "partial") {
                    continue;
                }
            }

            // Shadow-ASR: populate `parameters` from the arglist vector for
            // `defn` / `defn-` / `defmethod`. Types stay empty (Clojure is
            // dynamically typed); we capture the parameter NAMES in source
            // order. Multi-arity `defn`s use the FIRST arity's arglist (the
            // canonical signature surface).
            let parameters = if matches!(head_text.as_str(), "defn" | "defn-" | "defmethod") {
                extract_clojure_params(def.node, content)
            } else {
                Vec::new()
            };

            out.push(Symbol {
                file_id: 0,
                kind,
                start_line: line_of(def.node),
                end_line: end_line_of(def.node),
                parent_id: None,
                visibility,
                signature: Some(first_line(content, def.node)),
                name,
                parameters,
                ..Default::default()
            });
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
            let head_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "head");
            let outer_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.outer");
            let (Some(head), Some(outer)) = (head_cap, outer_cap) else {
                continue;
            };
            let head_text = node_text(head.node, content);
            match head_text {
                "ns" => walk_ns_form(outer.node, content, &mut out),
                "require" | "use" => walk_top_require(outer.node, content, &mut out, "require"),
                "import" => walk_top_require(outer.node, content, &mut out, "import"),
                _ => {}
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
                if cap_name != "ref.call" {
                    continue;
                }
                let node = cap.node;
                let target_raw = node_text(node, content).to_string();
                if target_raw.is_empty() || REF_DENY.contains(&target_raw.as_str()) {
                    continue;
                }
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
        out
    }

    fn extract_function_metrics(&self, content: &str) -> Vec<FunctionMetrics> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let mut out: Vec<FunctionMetrics> = Vec::new();
        collect_clojure_function_metrics(tree.root_node(), content, &mut out);
        out
    }
}

// ============================================================================
// Shadow-ASR: parameter extraction (Group 1d)
// ============================================================================

/// Clojure branching / binding head symbols that contribute a decision point.
/// `if`/`when`/`when-not`/`cond`/`condp`/`case`/`and`/`or`/`recur` per the plan.
const CLOJURE_DECISION_HEADS: &[&str] = &[
    "if", "if-not", "if-let", "if-some", "when", "when-not", "when-let", "when-some", "when-first",
    "cond", "cond->", "cond->>", "condp", "case", "and", "or", "recur",
];

/// Extract parameter NAMES from a `defn`/`defn-`/`defmethod`'s arglist vector.
///
/// Finds the first `vec_lit` among the def's named children (the first arity's
/// arglist), then collects each `sym_lit` as a positional parameter. A `&`
/// marks the following symbol as variadic (Clojure rest arg). Destructuring
/// forms (`[{:keys [a b]} c]`) are summarized: a map/vector binding becomes one
/// positional `Parameter` with `name=None` (anonymous destructure) so the arity
/// count stays correct without inventing names.
fn extract_clojure_params(def_node: Node<'_>, src: &str) -> Vec<Parameter> {
    // Locate the arglist: for `defn name [..]` it's the first vec_lit after the
    // name; for `defmethod name dispatch [..]` it's still the first vec_lit.
    let mut walker = def_node.walk();
    let arglist = def_node
        .named_children(&mut walker)
        .find(|c| c.kind() == "vec_lit");
    let Some(arglist) = arglist else {
        return Vec::new();
    };

    let mut params: Vec<Parameter> = Vec::new();
    let mut next_is_variadic = false;
    let mut position: u32 = 0;
    let mut w = arglist.walk();
    for child in arglist.named_children(&mut w) {
        match child.kind() {
            "sym_lit" => {
                let text = node_text(child, src);
                if text == "&" {
                    // Clojure rest-arg marker; the next symbol is variadic.
                    next_is_variadic = true;
                    continue;
                }
                // `_` is the conventional ignored binding — keep it as a named
                // positional so arity is preserved.
                params.push(Parameter {
                    position,
                    name: Some(text.to_string()),
                    is_variadic: next_is_variadic,
                    ..Default::default()
                });
                next_is_variadic = false;
                position += 1;
            }
            // Destructuring binding (`{:keys [...]}` or `[a b]`) — one
            // anonymous positional parameter.
            "map_lit" | "vec_lit" => {
                params.push(Parameter {
                    position,
                    name: None,
                    is_variadic: next_is_variadic,
                    type_raw: Some(node_text(child, src).chars().take(60).collect()),
                    ..Default::default()
                });
                next_is_variadic = false;
                position += 1;
            }
            _ => {}
        }
    }
    params
}

// ============================================================================
// extract_function_metrics — recursive walker (Group 1d)
// ============================================================================

/// Walk every `defn`/`defn-`/`defmethod`/`defmacro` form and score it.
fn collect_clojure_function_metrics(node: Node<'_>, src: &str, out: &mut Vec<FunctionMetrics>) {
    if node.kind() == "list_lit"
        && let Some(head) = node.named_child(0)
        && head.kind() == "sym_lit"
    {
        let head_text = node_text(head, src);
        if matches!(head_text, "defn" | "defn-" | "defmethod" | "defmacro")
            && let Some(name_node) = node.named_child(1)
            && name_node.kind() == "sym_lit"
        {
            let name = node_text(name_node, src).to_string();
            if !name.is_empty() {
                out.push(score_clojure_function(&name, node, src));
            }
        }
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_clojure_function_metrics(child, src, out);
    }
}

/// Score one Clojure function form.
fn score_clojure_function(name: &str, form: Node<'_>, src: &str) -> FunctionMetrics {
    use std::collections::HashMap;
    let mut decision_points: u32 = 0;
    let mut cognitive_increments: Vec<CognitiveIncrement> = Vec::new();
    let mut operators: HashMap<&'static str, u32> = HashMap::new();
    let mut operands: HashMap<String, u32> = HashMap::new();
    let mut npath_factors: Vec<u64> = Vec::new();
    let mut panic_paths: u32 = 0;

    walk_clojure_form(
        form,
        src,
        0,
        &mut decision_points,
        &mut cognitive_increments,
        &mut operators,
        &mut operands,
        &mut npath_factors,
        &mut panic_paths,
        name,
        true, // is the def form root — don't recurse into nested defn rows
    );

    let start_line = line_of(form);
    let end_line = end_line_of(form);
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
        comment_lines: 0,
        panic_paths,
        unsafe_blocks: 0,
    };
    complexity::score(&input)
}

#[allow(clippy::too_many_arguments)]
fn walk_clojure_form(
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
    is_root: bool,
) {
    let kind = node.kind();

    // Halstead leaf classification. In Clojure the "operators" are the head
    // symbols of list forms; everything else (other symbols, literals,
    // keywords) is an operand. We approximate by treating recognized special
    // forms / decision heads as operators and the rest as operands at the leaf.
    if node.child_count() == 0 || matches!(kind, "sym_lit" | "kwd_lit" | "num_lit" | "str_lit") {
        let text = node_text(node, src);
        if !text.is_empty() {
            if let Some(op) = match_clojure_operator(text) {
                *operators.entry(op).or_insert(0) += 1;
            } else {
                *operands.entry(text.to_string()).or_insert(0) += 1;
            }
        }
    }

    let mut new_depth = depth;
    // Inspect list forms' head symbol for decisions / throw / recursion.
    if kind == "list_lit"
        && let Some(head) = node.named_child(0)
        && head.kind() == "sym_lit"
    {
        let head_text = node_text(head, src);

        // Stop at nested `defn`/`defmethod`/`defmacro` so they get their own
        // metrics row (but always descend through the root form itself).
        if !is_root && matches!(head_text, "defn" | "defn-" | "defmethod" | "defmacro") {
            return;
        }

        if CLOJURE_DECISION_HEADS.contains(&head_text) {
            *decision_points = decision_points.saturating_add(1);
            // `cond`/`condp`/`case` fan out per clause-pair; approximate with
            // the number of branch children. `and`/`or` are logical sequences.
            match head_text {
                "and" | "or" => {
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::LogicalSequence,
                    });
                    npath_factors.push(2);
                }
                "recur" => {
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::BreakInFlow,
                    });
                }
                "cond" | "condp" | "case" => {
                    // Each pair of children after the head is one branch; the
                    // named-child count gives a reasonable branch estimate.
                    let branches = (node.named_child_count().saturating_sub(1) / 2).max(1) as u64;
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::NestedCondition,
                    });
                    npath_factors.push(branches.max(2));
                    new_depth = depth.saturating_add(1);
                }
                _ => {
                    cognitive_increments.push(CognitiveIncrement {
                        depth,
                        kind: CognitiveKind::NestedCondition,
                    });
                    npath_factors.push(2);
                    new_depth = depth.saturating_add(1);
                }
            }
        } else if matches!(head_text, "throw") {
            *panic_paths = panic_paths.saturating_add(1);
        } else if head_text == fn_name {
            // Direct self-call recursion.
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::Recursion,
            });
        }
    }

    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        walk_clojure_form(
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
            false,
        );
    }
}

/// Recognize Clojure head symbols / special forms classified as operators for
/// the Halstead η1 universe.
fn match_clojure_operator(s: &str) -> Option<&'static str> {
    const CLOJURE_OPERATOR_HEADS: &[&str] = &[
        "if", "if-not", "if-let", "if-some", "when", "when-not", "when-let", "when-some", "cond",
        "cond->", "cond->>", "condp", "case", "and", "or", "not", "recur", "loop", "let", "letfn",
        "fn", "fn*", "do", "doseq", "dotimes", "for", "defn", "defn-", "defmethod", "defmacro",
        "defmulti", "def", "ns", "throw", "try", "catch", "finally", "->", "->>", "as->", "some->",
        "some->>", "=", "==", "not=", "<", ">", "<=", ">=", "+", "-", "*", "/",
    ];
    CLOJURE_OPERATOR_HEADS.iter().copied().find(|t| *t == s)
}

/// Walk an `(ns name (:require ...) (:import ...))` form, emitting per-spec rows.
fn walk_ns_form(node: Node<'_>, src: &str, out: &mut Vec<Import>) {
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if child.kind() != "list_lit" {
            continue;
        }
        // Look at the first named child — should be a kwd_lit (:require / :use / :import).
        let Some(head) = child.named_child(0) else {
            continue;
        };
        if head.kind() != "kwd_lit" {
            continue;
        }
        let head_text = node_text(head, src);
        match head_text {
            ":require" | ":use" => {
                let line = line_of(child);
                walk_require_specs(child, src, out, line, false);
            }
            ":import" => {
                let line = line_of(child);
                walk_require_specs(child, src, out, line, true);
            }
            _ => {}
        }
    }
}

/// Walk a top-level (require '[...]) or (import '...) form.
fn walk_top_require(node: Node<'_>, src: &str, out: &mut Vec<Import>, kind: &str) {
    let line = line_of(node);
    let is_import = kind == "import";
    walk_require_specs(node, src, out, line, is_import);
}

/// Walk children of a `:require` / `:use` / `:import` list and emit rows.
fn walk_require_specs(
    parent: Node<'_>,
    src: &str,
    out: &mut Vec<Import>,
    line: u32,
    is_import: bool,
) {
    let mut walker = parent.walk();
    for spec in parent.named_children(&mut walker) {
        match spec.kind() {
            "vec_lit" => emit_vec_spec(spec, src, out, line, is_import),
            "quoting_lit" => {
                // (:use 'clojure.string) — quoted symbol or vector.
                if let Some(inner) = spec.named_child(0) {
                    match inner.kind() {
                        "sym_lit" => {
                            let target_raw = node_text(inner, src).to_string();
                            if !target_raw.is_empty() {
                                out.push(Import {
                                    target_raw,
                                    source_line: line,
                                    alias: None,
                                });
                            }
                        }
                        "vec_lit" => emit_vec_spec(inner, src, out, line, is_import),
                        _ => {}
                    }
                }
            }
            "sym_lit" => {
                // (:import java.util.Date) — bare symbol.
                let target_raw = node_text(spec, src).to_string();
                if !target_raw.is_empty()
                    && target_raw != ":require"
                    && target_raw != ":use"
                    && target_raw != ":import"
                {
                    out.push(Import {
                        target_raw,
                        source_line: line,
                        alias: None,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Emit imports from a vector spec: `[ns alias?]`, `[ns :as a :refer [...]]`,
/// `["str-mod" :as r]` (CLJS), `[java.util Date HashMap]` (:import).
fn emit_vec_spec(vec_node: Node<'_>, src: &str, out: &mut Vec<Import>, line: u32, is_import: bool) {
    let mut walker = vec_node.walk();
    let mut elements: Vec<Node<'_>> = Vec::new();
    for child in vec_node.named_children(&mut walker) {
        elements.push(child);
    }
    if elements.is_empty() {
        return;
    }

    // First element is the module name (sym_lit or str_lit).
    let first = elements[0];
    let module = match first.kind() {
        "sym_lit" => node_text(first, src).to_string(),
        "str_lit" => strip_quotes(node_text(first, src)),
        _ => return,
    };

    if is_import {
        // [java.util Date HashMap] → emit one row per class.
        for cls in elements.iter().skip(1) {
            if cls.kind() == "sym_lit" {
                let class_name = node_text(*cls, src).to_string();
                out.push(Import {
                    target_raw: format!("{}.{}", module, class_name),
                    source_line: line,
                    alias: None,
                });
            }
        }
        if elements.len() == 1 {
            // [java.util.Date] alone → emit just the module.
            out.push(Import {
                target_raw: module,
                source_line: line,
                alias: None,
            });
        }
        return;
    }

    // Require-style: scan for :as and :refer keywords.
    let mut alias: Option<String> = None;
    let mut refers: Vec<String> = Vec::new();
    let mut i = 1;
    while i < elements.len() {
        let elem = elements[i];
        if elem.kind() == "kwd_lit" {
            let kw = node_text(elem, src);
            match kw {
                ":as" if i + 1 < elements.len() => {
                    let nxt = elements[i + 1];
                    if nxt.kind() == "sym_lit" {
                        alias = Some(node_text(nxt, src).to_string());
                    }
                    i += 2;
                    continue;
                }
                ":refer" if i + 1 < elements.len() => {
                    let nxt = elements[i + 1];
                    if nxt.kind() == "vec_lit" {
                        let mut rw = nxt.walk();
                        for r in nxt.named_children(&mut rw) {
                            if r.kind() == "sym_lit" {
                                refers.push(node_text(r, src).to_string());
                            }
                        }
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }

    // Emit the module row with optional alias.
    out.push(Import {
        target_raw: module.clone(),
        source_line: line,
        alias: alias.clone(),
    });
    // Emit one extra row per :refer'd name (target=module.name).
    for r in refers {
        out.push(Import {
            target_raw: format!("{}.{}", module, r),
            source_line: line,
            alias: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
(ns my.app
  (:require [clojure.string :as str :refer [join]]
            [clojure.set]))

(defn greet [name]
  (str/join " " ["hello" name]))

(def MAX 100)

(defn- secret [] :hidden)

(defrecord Point [x y])
"#;

    #[test]
    fn extract_symbols_finds_ns_def_defn_record() {
        let syms = CLOJURE_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"my.app"), "names: {:?}", names);
        assert!(names.contains(&"greet"));
        assert!(names.contains(&"secret"));
        assert!(names.contains(&"MAX"));
        assert!(names.contains(&"Point"));
        let ns = syms.iter().find(|s| s.name == "my.app").unwrap();
        assert_eq!(ns.kind, SymbolKind::Module);
        let max = syms.iter().find(|s| s.name == "MAX").unwrap();
        assert_eq!(max.kind, SymbolKind::Const);
        let secret = syms.iter().find(|s| s.name == "secret").unwrap();
        assert_eq!(secret.visibility.as_deref(), Some("private"));
        let point = syms.iter().find(|s| s.name == "Point").unwrap();
        assert_eq!(point.kind, SymbolKind::Struct);
    }

    #[test]
    fn extract_imports_handles_alias_and_refer() {
        let imps = CLOJURE_BACKEND.extract_imports(SAMPLE);
        let pairs: Vec<(&str, Option<&str>)> = imps
            .iter()
            .map(|i| (i.target_raw.as_str(), i.alias.as_deref()))
            .collect();
        assert!(
            pairs.contains(&("clojure.string", Some("str"))),
            "imports: {:?}",
            pairs
        );
        assert!(
            pairs.contains(&("clojure.string.join", None)),
            "imports: {:?}",
            pairs
        );
        assert!(
            pairs.contains(&("clojure.set", None)),
            "imports: {:?}",
            pairs
        );
    }

    #[test]
    fn extract_references_filters_special_forms() {
        let refs = CLOJURE_BACKEND.extract_references(SAMPLE);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        // defn/defn-/def/ns/defrecord must be filtered.
        for denied in ["defn", "defn-", "def", "ns", "defrecord"] {
            assert!(!calls.contains(&denied), "denied form leaked: {}", denied);
        }
        // str/join is a real call.
        assert!(calls.contains(&"str/join"), "calls: {:?}", calls);
    }

    #[test]
    fn cljs_string_module_require() {
        let src = "(ns foo (:require [\"react\" :as r]))\n";
        let imps = CLOJURESCRIPT_BACKEND.extract_imports(src);
        let pairs: Vec<(&str, Option<&str>)> = imps
            .iter()
            .map(|i| (i.target_raw.as_str(), i.alias.as_deref()))
            .collect();
        assert!(pairs.contains(&("react", Some("r"))), "imports: {:?}", imps);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "((", "(defn"] {
            let _ = CLOJURE_BACKEND.extract_symbols(s);
            let _ = CLOJURE_BACKEND.extract_imports(s);
            let _ = CLOJURE_BACKEND.extract_references(s);
        }
    }

    #[test]
    fn language_names() {
        assert_eq!(CLOJURE_BACKEND.language_name(), "clojure");
        assert_eq!(CLOJURESCRIPT_BACKEND.language_name(), "clojurescript");
    }

    // ========================================================================
    // Shadow-ASR parameter extraction + function metrics (Group 1d)
    // ========================================================================

    #[test]
    fn defn_params_populate_names() {
        let src = "(defn greet [greeting name] (str greeting \" \" name))\n";
        let syms = CLOJURE_BACKEND.extract_symbols(src);
        let greet = syms.iter().find(|s| s.name == "greet").expect("greet");
        assert_eq!(greet.parameters.len(), 2, "params: {:?}", greet.parameters);
        assert_eq!(greet.parameters[0].name.as_deref(), Some("greeting"));
        assert_eq!(greet.parameters[1].name.as_deref(), Some("name"));
        // Types stay empty (dynamic typing).
        assert!(greet.parameters[0].type_raw.is_none());
    }

    #[test]
    fn defn_variadic_rest_arg_flagged() {
        let src = "(defn f [a & rest] rest)\n";
        let syms = CLOJURE_BACKEND.extract_symbols(src);
        let f = syms.iter().find(|s| s.name == "f").expect("f");
        // `&` is not a parameter; `a` + `rest` are, and `rest` is variadic.
        assert_eq!(f.parameters.len(), 2, "params: {:?}", f.parameters);
        assert_eq!(f.parameters[0].name.as_deref(), Some("a"));
        assert_eq!(f.parameters[1].name.as_deref(), Some("rest"));
        assert!(f.parameters[1].is_variadic);
    }

    #[test]
    fn defn_destructuring_is_anonymous_positional() {
        let src = "(defn handle [{:keys [x y]} z] (+ x y z))\n";
        let syms = CLOJURE_BACKEND.extract_symbols(src);
        let h = syms.iter().find(|s| s.name == "handle").expect("handle");
        // Two positional params: a destructure (anonymous) and `z`.
        assert_eq!(h.parameters.len(), 2, "params: {:?}", h.parameters);
        assert!(h.parameters[0].name.is_none(), "destructure is anonymous");
        assert_eq!(h.parameters[1].name.as_deref(), Some("z"));
    }

    #[test]
    fn defmethod_params_populate() {
        let src = "(defmethod area :circle [shape] (* 3.14 (:r shape)))\n";
        let syms = CLOJURESCRIPT_BACKEND.extract_symbols(src);
        let area = syms.iter().find(|s| s.name == "area").expect("area");
        assert_eq!(area.parameters.len(), 1, "params: {:?}", area.parameters);
        assert_eq!(area.parameters[0].name.as_deref(), Some("shape"));
    }

    #[test]
    fn cyclomatic_empty_defn_is_one() {
        let src = "(defn noop [] nil)\n";
        let m = CLOJURE_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1, "metrics: {:?}", m);
        assert_eq!(m[0].name, "noop");
        assert_eq!(m[0].cyclomatic, 1);
    }

    #[test]
    fn cyclomatic_counts_if_when_cond() {
        let src = r#"
(defn classify [x]
  (if (> x 0)
    (when (even? x)
      (cond
        (= x 2) :two
        (= x 4) :four
        :else :other))
    :negative))
"#;
        let m = CLOJURE_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        // if (+1), when (+1), cond (+1) → at least 3 decision points → CC >= 4.
        assert!(m[0].cyclomatic >= 4, "got CC = {}", m[0].cyclomatic);
    }

    #[test]
    fn cyclomatic_counts_and_or() {
        let src = "(defn check [a b c] (and a (or b c)))\n";
        let m = CLOJURE_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        // and (+1), or (+1) → CC >= 3.
        assert!(m[0].cyclomatic >= 3, "got CC = {}", m[0].cyclomatic);
    }

    #[test]
    fn throw_counts_as_panic_path() {
        let src = "(defn guard [x] (if (neg? x) (throw (ex-info \"bad\" {})) x))\n";
        let m = CLOJURE_BACKEND.extract_function_metrics(src);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].panic_paths, 1, "metrics: {:?}", m[0]);
    }

    #[test]
    fn nested_defns_score_independently() {
        let src = r#"
(defn outer [x] (if x 1 0))
(defn inner [y] (when y (or y 2)))
"#;
        let m = CLOJURE_BACKEND.extract_function_metrics(src);
        let names: Vec<&str> = m.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"outer"), "names: {:?}", names);
        assert!(names.contains(&"inner"));
        let outer = m.iter().find(|x| x.name == "outer").expect("outer");
        assert_eq!(outer.cyclomatic, 2); // one if
    }

    #[test]
    fn metrics_on_garbage_no_panic() {
        for s in ["", "(defn", "((", "   "] {
            let _ = CLOJURE_BACKEND.extract_function_metrics(s);
        }
    }
}
