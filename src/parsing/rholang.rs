//! Rholang language backend (Tier 0e). Uses the user's
//! `tree-sitter-rholang` grammar via path dependency in `Cargo.toml`.
//!
//! Rholang is a process-calculus language built on the asynchronous π-calculus
//! with reflection. The backend extracts:
//!
//! **Symbols** (`extract_symbols`):
//! - `contract name(args) = { … }` → `Function` (the primary definitional form).
//! - `let x = expr in { … }` → `Const` (immutable let binding).
//! - `new x in { … }` where `x` has **no** registry URI → `Module`
//!   (a fresh local channel that establishes a scope).
//!
//! **Imports** (`extract_imports`):
//! - `new x(\`rho:registry:lookup\`) in { … }` → registry-URI import; the
//!   stripped URI is the `target_raw`.
//!
//! **References** (`extract_references`):
//! - `chan!(...)` and `chan!?(...)` (sends) → `Call`.
//! - `receiver.name(args)` (method calls) → `Call`.
//! - `*x` (eval / dereference) → `Call`.
//! - `for(@m <- chan)` / `for(@m <= chan)` / `for(@m <<- chan)` (bind inputs)
//!   → `Call` to the channel being received from.
//!
//! **Function metrics** (`extract_function_metrics`):
//! - One row per `contract`, with cyclomatic complexity counting `else`
//!   clauses, `match` cases, `select` branches, `for` receipts, and the
//!   short-circuit `and`/`or` operators (matches Rust/Python convention).
//!
//! See: `/home/dylon/Workspace/f1r3fly.io/rholang-rs/rholang-tree-sitter/`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

#[path = "rholang/type_mapper.rs"]
mod type_mapper;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::complexity;
use crate::parsing::function_metrics::{
    CognitiveIncrement, CognitiveKind, FunctionMetrics, ScoringInput,
};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};
use crate::parsing::sync_ops::{
    FunctionSyncOps, ResourceConfidence, ResourceKind, SyncOp, SyncOpKind, SyncParadigm,
};

pub static RHOLANG_BACKEND: RholangBackend = RholangBackend;
pub struct RholangBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rholang::LANGUAGE.into())
            .expect("set_language rholang");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(contract) @contract.def
(let_var_decl (var) @let.name) @let.def
(name_decl (var) @chan.name !uri) @chan.def
"#;

const IMPORT_QUERY: &str = r#"
(name_decl
  (var) @import.alias
  uri: (uri_literal) @import.target) @import.decl
"#;

const REF_QUERY: &str = r#"
(send) @send.expr
(send_sync) @sendsync.expr
(method name: (var) @m.name) @m.expr
(eval) @eval.expr
(linear_bind input: (_) @bind.input) @bind.expr
(repeated_bind input: (_) @rbind.input) @rbind.expr
(peek_bind input: (_) @pbind.input) @pbind.expr
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), SYMBOL_QUERY)
            .expect("symbol query rholang")
    })
}
fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), IMPORT_QUERY)
            .expect("import query rholang")
    })
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), REF_QUERY).expect("ref query rholang")
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

/// Strip surrounding backticks from a `uri_literal` text (`\`rho:io:stdout\`` → `rho:io:stdout`).
fn strip_backticks(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('`') && t.ends_with('`') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'{' && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Extract a Rholang contract's name, walking through `_proc_var` or `quote`
/// wrappers to find a usable identifier.
fn contract_name(node: Node<'_>, src: &str) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    Some(extract_name_text(name_node, src))
}

fn extract_name_text(node: Node<'_>, src: &str) -> String {
    match node.kind() {
        "var" => node_text(node, src).to_string(),
        "_proc_var" | "proc_var" => {
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                if child.kind() == "var" {
                    return node_text(child, src).to_string();
                }
            }
            node_text(node, src).to_string()
        }
        "quote" => {
            // Quote can wrap a string_literal, var, or arbitrary process.
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                match child.kind() {
                    "var" => return node_text(child, src).to_string(),
                    "string_literal" => {
                        let raw = node_text(child, src);
                        return raw.trim_matches('"').to_string();
                    }
                    _ => {}
                }
            }
            // Fallback: raw text minus leading `@`.
            let raw = node_text(node, src).trim_start_matches('@');
            raw.to_string()
        }
        _ => node_text(node, src).to_string(),
    }
}

/// Walk a `send` / `send_sync` node and find its channel target identifier.
fn channel_target(node: Node<'_>, src: &str) -> Option<String> {
    let chan = node.child_by_field_name("channel")?;
    Some(extract_name_text(chan, src))
}

/// Resolve the channel target from a `linear_bind` / `repeated_bind` /
/// `peek_bind` input. The input node is one of `_source` family — either a
/// bare `name`, `receive_send_source` (`name?!`), or `send_receive_source`
/// (`name!?(…)`). In all three cases we extract the leading `name`.
fn bind_source_target(input: Node<'_>, src: &str) -> String {
    match input.kind() {
        "simple_source" | "receive_send_source" | "send_receive_source" => {
            // The `name` is the first named child for all three source kinds.
            let mut walker = input.walk();
            for child in input.named_children(&mut walker) {
                if child.kind() == "name" || child.kind() == "var" || child.kind() == "quote" {
                    return extract_name_text(child, src);
                }
            }
            String::new()
        }
        "name" | "var" | "quote" => extract_name_text(input, src),
        _ => {
            // Fallback: scan named children for the first name-like node.
            let mut walker = input.walk();
            for child in input.named_children(&mut walker) {
                let t = extract_name_text(child, src);
                if !t.is_empty() {
                    return t;
                }
            }
            String::new()
        }
    }
}

/// Static operator vocabulary tracked by Halstead scoring (η1 universe).
const RHOLANG_OPERATOR_KINDS: &[&str] = &[
    // Channel / send operators.
    "send_single",
    "send_multiple",
    "send_sync",
    // Bind arrows.
    "linear_bind",
    "repeated_bind",
    "peek_bind",
    // Control-flow keywords (counted via tokens / kind text).
    "if",
    "else",
    "match",
    "select",
    "new",
    "for",
    "let",
    "in",
    "bundle",
    "contract",
    "=>",
    // Binary expressions captured by node kind.
    "add",
    "sub",
    "mult",
    "div",
    "mod",
    "concat",
    "diff",
    "interpolation",
    "eq",
    "neq",
    "lt",
    "lte",
    "gt",
    "gte",
    "matches",
    "and",
    "or",
    "not",
    "neg",
    "negation",
    "disjunction",
    "conjunction",
    "eval",
    "method",
];

/// Walk a contract subtree to count branching constructs + accumulate
/// Halstead vocabulary, producing the `ScoringInput` consumed by the
/// language-agnostic scorer.
fn compute_contract_metrics(name: String, node: Node<'_>, src: &str) -> FunctionMetrics {
    use std::collections::HashMap;
    let mut decision_points: u32 = 0;
    let mut cognitive_increments: Vec<CognitiveIncrement> = Vec::new();
    let mut operators: HashMap<&'static str, u32> = HashMap::new();
    let mut operands: HashMap<String, u32> = HashMap::new();
    let mut npath_factors: Vec<u64> = Vec::new();

    let body = node.child_by_field_name("proc").unwrap_or(node);
    walk_rholang_proc(
        body,
        src,
        0,
        &mut decision_points,
        &mut cognitive_increments,
        &mut operators,
        &mut operands,
        &mut npath_factors,
    );

    let start_line = line_of(node);
    let end_line = end_line_of(node);
    let source_lines = end_line.saturating_sub(start_line) + 1;
    let input = ScoringInput {
        name: name.as_str(),
        start_line,
        end_line,
        decision_points,
        cognitive_increments,
        operators,
        operands,
        npath_factors,
        source_lines,
        comment_lines: 0,
        // Rholang has no `panic!`/`unwrap`/`expect` analog; the scoring input
        // surfaces `0` here since panic-path counting is Rust/Python-specific.
        panic_paths: 0,
        unsafe_blocks: 0,
    };
    complexity::score(&input)
}

/// Match a Rholang node kind to its canonical Halstead operator token, or
/// `None` if the kind is not an operator.
fn match_rholang_operator(kind: &str) -> Option<&'static str> {
    RHOLANG_OPERATOR_KINDS.iter().copied().find(|k| *k == kind)
}

#[allow(clippy::too_many_arguments)]
fn walk_rholang_proc(
    node: Node<'_>,
    src: &str,
    depth: u8,
    decision_points: &mut u32,
    cognitive_increments: &mut Vec<CognitiveIncrement>,
    operators: &mut std::collections::HashMap<&'static str, u32>,
    operands: &mut std::collections::HashMap<String, u32>,
    npath_factors: &mut Vec<u64>,
) {
    let kind = node.kind();

    // Halstead operator counting by node kind.
    if let Some(op) = match_rholang_operator(kind) {
        *operators.entry(op).or_insert(0) += 1;
    }

    // Halstead operand counting from leaf tokens.
    if node.child_count() == 0 {
        let text = node_text(node, src);
        if !text.is_empty()
            && matches!(
                kind,
                "var"
                    | "long_literal"
                    | "signed_int_literal"
                    | "unsigned_int_literal"
                    | "bigint_literal"
                    | "bigrat_literal"
                    | "float_literal"
                    | "fixed_point_literal"
                    | "string_literal"
                    | "uri_literal"
                    | "bool_literal"
                    | "nil"
                    | "wildcard"
            )
        {
            *operands.entry(text.to_string()).or_insert(0) += 1;
        }
    }

    // Decision-point + cognitive accounting per node kind.
    let mut new_depth = depth;
    let mut entered_nest = false;
    match kind {
        "ifElse" => {
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            // If an `else` clause is present (4 named children: cond,
            // consequence, alternative), bump cyclomatic by one more.
            let mut walker = node.walk();
            let alternative = node.named_children(&mut walker).nth(2);
            if alternative.is_some() {
                *decision_points = decision_points.saturating_add(1);
                npath_factors.push(2);
            } else {
                npath_factors.push(2);
            }
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "match" => {
            // Each `case` in the `cases` alias is a decision point.
            let mut walker = node.walk();
            let case_count = node
                .named_children(&mut walker)
                .flat_map(|c| {
                    let mut w = c.walk();
                    c.named_children(&mut w)
                        .map(|cc| cc.kind().to_string())
                        .collect::<Vec<_>>()
                })
                .filter(|k| k == "case")
                .count() as u32;
            let cases = case_count.max(1);
            *decision_points = decision_points.saturating_add(cases);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(cases.max(2) as u64);
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "choice" => {
            // `select { branch ... }` — each branch is a decision point.
            let mut walker = node.walk();
            let branch_count = node
                .named_children(&mut walker)
                .flat_map(|c| {
                    let mut w = c.walk();
                    c.named_children(&mut w)
                        .map(|cc| cc.kind().to_string())
                        .collect::<Vec<_>>()
                })
                .filter(|k| k == "branch")
                .count() as u32;
            let branches = branch_count.max(1);
            *decision_points = decision_points.saturating_add(branches);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(branches.max(2) as u64);
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "input" => {
            // `for(receipt ; receipt ; …) { … }` — each receipt is a parallel
            // branch the runtime can choose between.
            let mut walker = node.walk();
            let receipt_count = node
                .named_children(&mut walker)
                .flat_map(|c| {
                    let mut w = c.walk();
                    c.named_children(&mut w)
                        .map(|cc| cc.kind().to_string())
                        .collect::<Vec<_>>()
                })
                .filter(|k| k == "receipt")
                .count() as u32;
            let receipts = receipt_count.max(1);
            *decision_points = decision_points.saturating_add(receipts);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::NestedCondition,
            });
            npath_factors.push(receipts.max(2) as u64);
            new_depth = depth.saturating_add(1);
            entered_nest = true;
        }
        "and" | "or" => {
            // Short-circuit operators: one decision point per occurrence.
            *decision_points = decision_points.saturating_add(1);
            cognitive_increments.push(CognitiveIncrement {
                depth,
                kind: CognitiveKind::LogicalSequence,
            });
        }
        _ => {}
    }

    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        walk_rholang_proc(
            child,
            src,
            new_depth,
            decision_points,
            cognitive_increments,
            operators,
            operands,
            npath_factors,
        );
    }

    let _ = entered_nest;
}

/// Push one message-passing sync op. Rholang has no shared-memory locks, so
/// every op is `paradigm = Message` on a `Channel`; identity comes from
/// `channel_target` / `bind_source_target` (confidence `ChannelName` when a
/// name resolved, else `Unknown`).
fn push_msg_op(
    ops: &mut Vec<SyncOp>,
    seq: &mut u32,
    kind: SyncOpKind,
    key: Option<String>,
    depth: u32,
    line: u32,
) {
    let conf = if key.is_some() {
        ResourceConfidence::ChannelName.value()
    } else {
        ResourceConfidence::Unknown.value()
    };
    ops.push(SyncOp {
        seq: *seq,
        op_kind: kind,
        resource_kind: ResourceKind::Channel,
        paradigm: SyncParadigm::Message,
        resource_key: key,
        resource_confidence: conf,
        nesting_depth: depth,
        guard_id: None,
        line,
    });
    *seq += 1;
}

/// Ordered DFS over a contract body, emitting channel send/recv sync ops in
/// source order. Does not descend into nested `contract` / `name_decl` nodes —
/// those are separate symbols with their own skeletons. `par` (`|`) composition
/// and `eval` (`*x`) are intentionally NOT recorded as ops in v1: the
/// channel-deadlock analysis is driven by send/recv matching, and treating
/// ubiquitous par-composition as an explicit spawn op would flood the net.
fn walk_sync(
    node: Node<'_>,
    src: &str,
    depth: u32,
    seq: &mut u32,
    ops: &mut Vec<SyncOp>,
    is_root: bool,
) {
    if !is_root && matches!(node.kind(), "contract" | "name_decl") {
        return;
    }
    match node.kind() {
        "send" => {
            let key = channel_target(node, src).filter(|s| !s.is_empty());
            let kind = match node.child_by_field_name("send_type").map(|n| n.kind()) {
                Some("send_multiple") => SyncOpKind::SendPersistent,
                _ => SyncOpKind::Send,
            };
            push_msg_op(ops, seq, kind, key, depth, line_of(node));
        }
        "send_sync" => {
            let key = channel_target(node, src).filter(|s| !s.is_empty());
            push_msg_op(ops, seq, SyncOpKind::Send, key, depth, line_of(node));
        }
        "linear_bind" | "repeated_bind" | "peek_bind" => {
            let key = node
                .child_by_field_name("input")
                .map(|i| bind_source_target(i, src))
                .filter(|s| !s.is_empty());
            let kind = if node.kind() == "repeated_bind" {
                SyncOpKind::RecvPersistent
            } else {
                // linear and peek both consume-or-read a single message; v1
                // models peek as a (non-persistent) receive.
                SyncOpKind::Recv
            };
            push_msg_op(ops, seq, kind, key, depth, line_of(node));
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_sync(child, src, depth + 1, seq, ops, false);
    }
}

/// Build the ordered synchronization skeleton for one Rholang contract body.
fn sync_ops_for_body(body: Node<'_>, src: &str) -> Vec<SyncOp> {
    let mut ops = Vec::new();
    let mut seq = 0u32;
    walk_sync(body, src, 0, &mut seq, &mut ops, true);
    ops
}

impl LanguageBackend for RholangBackend {
    fn language_name(&self) -> &'static str {
        "rholang"
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
            // Each match has exactly one of the three capture sets:
            //   { contract.def }, { let.def, let.name }, or { chan.def, chan.name }
            let contract_def = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "contract.def");
            if let Some(cap) = contract_def {
                let node = cap.node;
                let Some(name) = contract_name(node, content) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                // Shadow-ASR: extract parameters from `formals: (names ...)`
                // and walk the body for channel-shaped effects.
                let parameters = node
                    .child_by_field_name("formals")
                    .map(|f| type_mapper::parameters_from_formals(f, content))
                    .unwrap_or_default();
                let return_type = Some(type_mapper::return_type_for_contract());
                let effects = node
                    .child_by_field_name("proc")
                    .map(|p| type_mapper::effects_for_contract_body(p, content))
                    .unwrap_or_else(|| {
                        vec![
                            crate::parsing::type_tags::vocabulary::EFFECT_CONTRACT_DEFINE
                                .to_string(),
                        ]
                    });
                out.push(Symbol {
                    file_id: 0,
                    kind: SymbolKind::Function,
                    start_line: line_of(node),
                    end_line: end_line_of(node),
                    parent_id: None,
                    visibility: None,
                    signature: Some(first_line(content, node)),
                    name,
                    parameters,
                    return_type,
                    effects,
                    scope_depth: Some(0),
                    ..Default::default()
                });
                continue;
            }

            let let_def = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "let.def");
            let let_name = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "let.name");
            if let (Some(def), Some(name_cap)) = (let_def, let_name) {
                let name = node_text(name_cap.node, content).to_string();
                if name.is_empty() {
                    continue;
                }
                out.push(Symbol {
                    file_id: 0,
                    kind: SymbolKind::Const,
                    start_line: line_of(def.node),
                    end_line: end_line_of(def.node),
                    parent_id: None,
                    visibility: None,
                    signature: Some(first_line(content, def.node)),
                    name,
                    ..Default::default()
                });
                continue;
            }

            let chan_def = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "chan.def");
            let chan_name = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "chan.name");
            if let (Some(def), Some(name_cap)) = (chan_def, chan_name) {
                let name = node_text(name_cap.node, content).to_string();
                if name.is_empty() {
                    continue;
                }
                // Shadow-ASR: channel decl carries channel/name/process tags
                // and the body's effects.
                let return_type = Some(type_mapper::return_type_for_channel(false));
                let effects = def
                    .node
                    .child_by_field_name("proc")
                    .map(|p| type_mapper::effects_for_block(p, content))
                    .unwrap_or_default();
                out.push(Symbol {
                    file_id: 0,
                    kind: SymbolKind::Module,
                    start_line: line_of(def.node),
                    end_line: end_line_of(def.node),
                    parent_id: None,
                    visibility: None,
                    signature: Some(first_line(content, def.node)),
                    name,
                    return_type,
                    effects,
                    scope_depth: Some(0),
                    ..Default::default()
                });
                continue;
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
            let alias_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.alias");
            let target_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.target");
            let decl_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.decl");
            if let (Some(alias), Some(target)) = (alias_cap, target_cap) {
                let line = decl_cap
                    .map(|c| line_of(c.node))
                    .unwrap_or_else(|| line_of(target.node));
                let alias_text = node_text(alias.node, content).to_string();
                let target_raw = strip_backticks(node_text(target.node, content)).to_string();
                if !target_raw.is_empty() {
                    out.push(Import {
                        target_raw,
                        source_line: line,
                        alias: if alias_text.is_empty() {
                            None
                        } else {
                            Some(alias_text)
                        },
                    });
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
            // Send / send_sync: channel target is in the `channel` field.
            let send_cap = m.captures.iter().find(|c| {
                let n = q.capture_names()[c.index as usize];
                n == "send.expr" || n == "sendsync.expr"
            });
            if let Some(cap) = send_cap
                && let Some(target_raw) = channel_target(cap.node, content)
                && !target_raw.is_empty()
            {
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw,
                    ref_kind: SymbolRefKind::Call,
                    source_line: line_of(cap.node),
                });
                continue;
            }

            // Method call: name capture holds the method identifier.
            let method_expr = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "m.expr");
            let method_name = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "m.name");
            if let (Some(expr), Some(name)) = (method_expr, method_name) {
                let target = node_text(name.node, content).to_string();
                if !target.is_empty() {
                    out.push(SymbolReference {
                        source_file_id: 0,
                        source_symbol_id: None,
                        target_file_id: None,
                        target_symbol_id: None,
                        target_raw: target,
                        ref_kind: SymbolRefKind::Call,
                        source_line: line_of(expr.node),
                    });
                }
                continue;
            }

            // Eval (`*name`): the first named child is the name (a `var`,
            // `quote`, or `wildcard` — `name` is inlined in the grammar).
            let eval_expr = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "eval.expr");
            if let Some(expr) = eval_expr
                && let Some(name_node) = expr.node.named_child(0)
            {
                let target = extract_name_text(name_node, content);
                if !target.is_empty() {
                    out.push(SymbolReference {
                        source_file_id: 0,
                        source_symbol_id: None,
                        target_file_id: None,
                        target_symbol_id: None,
                        target_raw: target,
                        ref_kind: SymbolRefKind::Call,
                        source_line: line_of(expr.node),
                    });
                }
                continue;
            }

            // for-comprehension bind inputs: the channel being received from.
            // Linear (`<-`), repeated (`<=`), and peek (`<<-`) all participate.
            let bind_expr = m.captures.iter().find(|c| {
                matches!(
                    q.capture_names()[c.index as usize],
                    "bind.expr" | "rbind.expr" | "pbind.expr"
                )
            });
            let bind_input = m.captures.iter().find(|c| {
                matches!(
                    q.capture_names()[c.index as usize],
                    "bind.input" | "rbind.input" | "pbind.input"
                )
            });
            if let (Some(expr), Some(input)) = (bind_expr, bind_input) {
                let target = bind_source_target(input.node, content);
                if !target.is_empty() {
                    out.push(SymbolReference {
                        source_file_id: 0,
                        source_symbol_id: None,
                        target_file_id: None,
                        target_symbol_id: None,
                        target_raw: target,
                        ref_kind: SymbolRefKind::Call,
                        source_line: line_of(expr.node),
                    });
                }
                continue;
            }
        }
        out
    }

    fn extract_function_metrics(&self, content: &str) -> Vec<FunctionMetrics> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let mut out: Vec<FunctionMetrics> = Vec::new();
        let q = symbol_query();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            let Some(cap) = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "contract.def")
            else {
                continue;
            };
            let node = cap.node;
            let Some(name) = contract_name(node, content) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let metric = compute_contract_metrics(name, node, content);
            out.push(metric);
        }
        out
    }

    fn extract_sync_ops(&self, content: &str) -> Vec<FunctionSyncOps> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let mut out: Vec<FunctionSyncOps> = Vec::new();
        let q = symbol_query();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            // Only contracts carry a function-like body; `let`/channel decls are
            // keyed elsewhere. Mirrors `extract_function_metrics`.
            let Some(cap) = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "contract.def")
            else {
                continue;
            };
            let node = cap.node;
            let Some(name) = contract_name(node, content) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let body = node.child_by_field_name("proc").unwrap_or(node);
            let ops = sync_ops_for_body(body, content);
            if !ops.is_empty() {
                out.push(FunctionSyncOps {
                    function: name,
                    start_line: line_of(node),
                    end_line: end_line_of(node),
                    ops,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_WORLD: &str = "new stdout(`rho:io:stdout`) in {\n  \
contract helloworld(@name) = {\n    \
  stdout!(\"hello, \" ++ name)\n  \
}\n  \
|\n  \
helloworld!(\"world\")\n  \
|\n  \
helloworld!(\"world2\")\n\
}\n";

    #[test]
    fn extract_sync_ops_records_send_and_recv() {
        let src = "new chan, ack in {\n  \
                   contract worker(@job) = {\n    \
                     for(@msg <- chan) {\n      \
                       ack!(msg)\n    \
                     }\n  \
                   }\n\
                   }\n";
        let fns = RholangBackend.extract_sync_ops(src);
        let ops: Vec<SyncOp> = fns.into_iter().flat_map(|f| f.ops).collect();
        assert!(
            ops.iter().any(|o| o.op_kind == SyncOpKind::Recv
                && o.resource_key.as_deref() == Some("chan")),
            "expected a linear recv on `chan`: {ops:?}"
        );
        assert!(
            ops.iter()
                .any(|o| o.op_kind == SyncOpKind::Send && o.resource_key.as_deref() == Some("ack")),
            "expected a send on `ack`: {ops:?}"
        );
        assert!(
            ops.iter().all(|o| o.paradigm == SyncParadigm::Message),
            "Rholang ops are all message-passing"
        );
    }

    #[test]
    fn extract_symbols_finds_contract() {
        let syms = RHOLANG_BACKEND.extract_symbols(HELLO_WORLD);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"helloworld"), "names: {:?}", names);
        let helloworld = syms.iter().find(|s| s.name == "helloworld").unwrap();
        assert_eq!(helloworld.kind, SymbolKind::Function);
    }

    #[test]
    fn extract_imports_handles_registry_uri() {
        let imps = RHOLANG_BACKEND.extract_imports(HELLO_WORLD);
        assert!(
            imps.iter().any(|i| i.target_raw == "rho:io:stdout"),
            "imports: {:?}",
            imps
        );
        assert!(imps.iter().any(|i| i.alias.as_deref() == Some("stdout")));
    }

    #[test]
    fn extract_references_finds_sends() {
        let refs = RHOLANG_BACKEND.extract_references(HELLO_WORLD);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(calls.contains(&"helloworld"), "calls: {:?}", calls);
        assert!(calls.contains(&"stdout"), "calls: {:?}", calls);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "new x in {", "contract foo("] {
            let _ = RHOLANG_BACKEND.extract_symbols(s);
            let _ = RHOLANG_BACKEND.extract_imports(s);
            let _ = RHOLANG_BACKEND.extract_references(s);
            let _ = RHOLANG_BACKEND.extract_function_metrics(s);
        }
    }

    #[test]
    fn language_name_is_rholang() {
        assert_eq!(RHOLANG_BACKEND.language_name(), "rholang");
    }

    #[test]
    fn extract_symbols_finds_let_var_decl() {
        let src = "let x = 5 in { Nil }\n";
        let syms = RHOLANG_BACKEND.extract_symbols(src);
        let x = syms.iter().find(|s| s.name == "x");
        assert!(x.is_some(), "syms: {:?}", syms);
        assert_eq!(x.expect("x").kind, SymbolKind::Const);
    }

    #[test]
    fn extract_symbols_finds_local_channel() {
        let src = "new ch in { ch!(42) }\n";
        let syms = RHOLANG_BACKEND.extract_symbols(src);
        let ch = syms.iter().find(|s| s.name == "ch");
        assert!(ch.is_some(), "syms: {:?}", syms);
        assert_eq!(ch.expect("ch").kind, SymbolKind::Module);
    }

    #[test]
    fn extract_symbols_skips_registry_new() {
        let src = "new stdout(`rho:io:stdout`) in { stdout!(\"hi\") }\n";
        let syms = RHOLANG_BACKEND.extract_symbols(src);
        // The URI-bearing name_decl should NOT produce a Module symbol.
        assert!(
            !syms.iter().any(|s| s.name == "stdout"),
            "registry-URI new should not emit Module symbol; syms: {:?}",
            syms
        );
    }

    #[test]
    fn extract_references_finds_method() {
        let src = "contract greet(@name) = { name.length() }\n";
        let refs = RHOLANG_BACKEND.extract_references(src);
        let methods: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(methods.contains(&"length"), "refs: {:?}", refs);
    }

    #[test]
    fn extract_references_finds_eval() {
        let src = "new ch in { *ch }\n";
        let refs = RHOLANG_BACKEND.extract_references(src);
        let evals: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(evals.contains(&"ch"), "refs: {:?}", refs);
    }

    #[test]
    fn extract_references_finds_bind_input() {
        let src = "new chan in { for(@m <- chan) { Nil } }\n";
        let refs = RHOLANG_BACKEND.extract_references(src);
        let binds: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(binds.contains(&"chan"), "refs: {:?}", refs);
    }

    #[test]
    fn function_metrics_counts_match_cases() {
        let src = "contract m(@x) = { match x { 1 => Nil 2 => Nil _ => Nil } }\n";
        let metrics = RHOLANG_BACKEND.extract_function_metrics(src);
        let m = metrics
            .iter()
            .find(|m| m.name == "m")
            .expect("contract metric");
        // baseline (1) + 3 match cases = at least 4.
        assert!(m.cyclomatic >= 4, "cyclomatic too low: {}", m.cyclomatic);
    }

    #[test]
    fn function_metrics_counts_if_else() {
        let src = "contract f(@cond, @y) = { if (cond) { y!(1) } else { y!(2) } }\n";
        let metrics = RHOLANG_BACKEND.extract_function_metrics(src);
        let f = metrics
            .iter()
            .find(|m| m.name == "f")
            .expect("contract metric");
        // baseline (1) + if (1) + else (1) = 3+.
        assert!(f.cyclomatic >= 3, "cyclomatic too low: {}", f.cyclomatic);
    }

    #[test]
    fn function_metrics_counts_select_branches() {
        let src = "contract s(@a, @b) = { select { x <- a => x!(1); y <- b => y!(2) } }\n";
        let metrics = RHOLANG_BACKEND.extract_function_metrics(src);
        let s = metrics
            .iter()
            .find(|m| m.name == "s")
            .expect("contract metric");
        // baseline (1) + 2 branches = 3+.
        assert!(s.cyclomatic >= 3, "cyclomatic too low: {}", s.cyclomatic);
    }
}
