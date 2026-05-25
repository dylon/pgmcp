//! Rust intraprocedural def-use extraction for taint analysis (Phase 2.1).
//!
//! Walks each function body with `syn`, building one
//! [`FunctionDataflow`](crate::parsing::dataflow::FunctionDataflow): a fresh
//! flow node per variable *definition* (SSA-style, so reassignment is
//! directional), flow edges from an assignment's RHS value-nodes to the LHS
//! node, taint **sources** (env/argv/stdin/request calls), and **sinks**
//! (Command/SQL/eval/deserialize/path calls — including builder chains like
//! `Command::new(..).arg(tainted)`). Inline **sanitizer** calls clear taint by
//! yielding no carried nodes, so a sanitized value reaches no sink.
//!
//! This is intraprocedural and value-provenance based (not full alias
//! analysis), but it is a genuine source→sink *flow* check — the rigor upgrade
//! over the old regex source/sink co-occurrence.

use std::collections::HashMap;

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Block, Expr, ExprCall, ExprMethodCall, ImplItem, Item, Pat, Stmt};

use crate::code_analysis::taint_spec;
use crate::parsing::dataflow::{CallSite, FlowNode, FunctionDataflow, TaintSink, TaintSource};

/// Parse `content` and emit one `FunctionDataflow` per non-trivial function
/// (one with at least one source and one sink). Parse errors yield `Vec::new()`.
pub fn extract(content: &str) -> Vec<FunctionDataflow> {
    let file = match syn::parse_file(content) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    out
}

fn collect_item(item: &Item, out: &mut Vec<FunctionDataflow>) {
    match item {
        Item::Fn(f) => push_fn(&f.sig, &f.block, out),
        Item::Impl(im) => {
            for ii in &im.items {
                if let ImplItem::Fn(m) = ii {
                    push_fn(&m.sig, &m.block, out);
                }
            }
        }
        Item::Trait(t) => {
            for ti in &t.items {
                if let syn::TraitItem::Fn(m) = ti
                    && let Some(blk) = &m.default
                {
                    push_fn(&m.sig, blk, out);
                }
            }
        }
        Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for it in items {
                    collect_item(it, out);
                }
            }
        }
        _ => {}
    }
}

fn push_fn(sig: &syn::Signature, block: &Block, out: &mut Vec<FunctionDataflow>) {
    let mut v = DfVisitor::new(sig.ident.to_string(), line_of(sig.ident.span()));
    // Parameters become initial (untainted) nodes so uses resolve; recorded in
    // declaration order for interprocedural param→sink summaries (Phase 3.4).
    // `self`/receiver is skipped, so positional indices align with free-function
    // call-site arguments (the calls the extractor records).
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pt) = input
            && let Some(name) = pat_ident(&pt.pat)
        {
            let n = v.new_node();
            v.params.push(n);
            v.vars.insert(name, n);
        }
    }
    v.walk_fn_body(block);
    let df = v.finish(line_of(block.brace_token.span.close()));
    // Keep a function when it has an intraprocedural flow to analyze
    // (`!is_trivial` = source+sink), OR when it participates in interprocedural
    // taint (Phase 3.4): as a CALLEE that could route a param to a sink
    // (params + sink/call), or as a CALLER that could pass tainted data into a
    // callee (sources + calls). `is_trivial` (source+sink) alone would drop
    // both the sink-only helper and the source-only caller.
    let callee_relevant = !df.params.is_empty() && (!df.sinks.is_empty() || !df.calls.is_empty());
    let caller_relevant = !df.sources.is_empty() && !df.calls.is_empty();
    if !df.is_trivial() || callee_relevant || caller_relevant {
        out.push(df);
    }
}

/// Result of evaluating an expression: the flow nodes whose taint it carries,
/// and (for builder-style sinks) the propagated sink context so a later
/// `.arg(tainted)` in the chain registers the flow.
struct Eval {
    nodes: Vec<FlowNode>,
    sink_ctx: Option<(&'static str, String)>,
}

impl Eval {
    fn plain(nodes: Vec<FlowNode>) -> Self {
        Eval {
            nodes,
            sink_ctx: None,
        }
    }
    fn empty() -> Self {
        Eval {
            nodes: Vec::new(),
            sink_ctx: None,
        }
    }
}

struct DfVisitor {
    function: String,
    start_line: u32,
    counter: FlowNode,
    vars: HashMap<String, FlowNode>,
    edges: Vec<(FlowNode, FlowNode)>,
    sources: Vec<TaintSource>,
    sinks: Vec<TaintSink>,
    params: Vec<FlowNode>,
    return_nodes: Vec<FlowNode>,
    calls: Vec<CallSite>,
}

impl DfVisitor {
    fn new(function: String, start_line: u32) -> Self {
        DfVisitor {
            function,
            start_line,
            counter: 0,
            vars: HashMap::new(),
            edges: Vec::new(),
            sources: Vec::new(),
            sinks: Vec::new(),
            params: Vec::new(),
            return_nodes: Vec::new(),
            calls: Vec::new(),
        }
    }

    fn new_node(&mut self) -> FlowNode {
        let n = self.counter;
        self.counter += 1;
        n
    }

    fn finish(self, end_line: u32) -> FunctionDataflow {
        FunctionDataflow {
            function: self.function,
            start_line: self.start_line,
            end_line,
            node_count: self.counter,
            flow_edges: self.edges,
            sources: self.sources,
            sanitized: Vec::new(), // Rust clears taint inline (sanitizer → no carried nodes).
            sinks: self.sinks,
            params: self.params,
            return_nodes: self.return_nodes,
            calls: self.calls,
        }
    }

    fn walk_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
    }

    /// Walk a function's top-level body, treating a trailing tail expression
    /// (the implicit return) as a return site so its value-nodes are recorded
    /// in `return_nodes` for interprocedural param→return summaries (Phase 3.4).
    fn walk_fn_body(&mut self, block: &Block) {
        let last = block.stmts.len().saturating_sub(1);
        for (i, stmt) in block.stmts.iter().enumerate() {
            if i == last
                && let Stmt::Expr(e, None) = stmt
            {
                let r = self.eval(e);
                self.return_nodes.extend(r.nodes);
                continue;
            }
            self.walk_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Local(local) => {
                let r = match &local.init {
                    Some(init) => self.eval(&init.expr),
                    None => Eval::empty(),
                };
                if let Some(name) = pat_ident(&local.pat) {
                    let node = self.new_node();
                    for n in r.nodes {
                        self.edges.push((n, node));
                    }
                    self.vars.insert(name, node);
                }
            }
            Stmt::Expr(expr, _) => {
                if let Expr::Assign(a) = expr {
                    let r = self.eval(&a.right);
                    if let Some(name) = expr_path_ident(&a.left) {
                        let node = self.new_node();
                        for n in r.nodes {
                            self.edges.push((n, node));
                        }
                        self.vars.insert(name, node);
                    } else {
                        self.eval(&a.left);
                    }
                } else {
                    self.eval(expr);
                }
            }
            Stmt::Macro(m) => {
                self.eval_macro(&m.mac);
            }
            Stmt::Item(_) => {}
        }
    }

    /// Evaluate an expression for both its carried taint and its side effects
    /// (registering nested sources/sinks, walking control-flow bodies).
    fn eval(&mut self, expr: &Expr) -> Eval {
        match expr {
            Expr::Path(p) => match path_single_ident(&p.path) {
                Some(name) => match self.vars.get(&name) {
                    Some(&n) => Eval::plain(vec![n]),
                    None => Eval::empty(),
                },
                None => Eval::empty(),
            },
            Expr::Lit(_) => Eval::empty(),
            Expr::Reference(r) => self.eval(&r.expr),
            Expr::Paren(p) => self.eval(&p.expr),
            Expr::Group(g) => self.eval(&g.expr),
            Expr::Unary(u) => self.eval(&u.expr),
            Expr::Try(t) => self.eval(&t.expr),
            Expr::Await(a) => self.eval(&a.base),
            Expr::Cast(c) => Eval::plain(self.eval(&c.expr).nodes),
            Expr::Field(f) => Eval::plain(self.eval(&f.base).nodes),
            Expr::Index(i) => {
                let mut nodes = self.eval(&i.expr).nodes;
                nodes.extend(self.eval(&i.index).nodes);
                Eval::plain(nodes)
            }
            Expr::Binary(b) => {
                let mut nodes = self.eval(&b.left).nodes;
                nodes.extend(self.eval(&b.right).nodes);
                Eval::plain(nodes)
            }
            Expr::Assign(a) => {
                // assignment-as-expression: record the def, carry RHS value.
                let r = self.eval(&a.right);
                if let Some(name) = expr_path_ident(&a.left) {
                    let node = self.new_node();
                    for n in &r.nodes {
                        self.edges.push((*n, node));
                    }
                    self.vars.insert(name, node);
                }
                Eval::plain(r.nodes)
            }
            Expr::Macro(m) => self.eval_macro(&m.mac),
            Expr::Call(c) => self.eval_call(c),
            Expr::MethodCall(m) => self.eval_method(m),
            Expr::Array(a) => {
                Eval::plain(a.elems.iter().flat_map(|e| self.eval(e).nodes).collect())
            }
            Expr::Tuple(t) => {
                Eval::plain(t.elems.iter().flat_map(|e| self.eval(e).nodes).collect())
            }
            Expr::Struct(s) => {
                let mut nodes = Vec::new();
                for f in &s.fields {
                    nodes.extend(self.eval(&f.expr).nodes);
                }
                Eval::plain(nodes)
            }
            // Control flow: walk bodies for side effects (sinks/defs inside);
            // don't track the branch's value taint (conservative).
            Expr::Block(b) => {
                self.walk_block(&b.block);
                Eval::empty()
            }
            Expr::If(i) => {
                self.eval(&i.cond);
                self.walk_block(&i.then_branch);
                if let Some((_, els)) = &i.else_branch {
                    self.eval(els);
                }
                Eval::empty()
            }
            Expr::Match(m) => {
                self.eval(&m.expr);
                for arm in &m.arms {
                    self.eval(&arm.body);
                }
                Eval::empty()
            }
            Expr::While(w) => {
                self.eval(&w.cond);
                self.walk_block(&w.body);
                Eval::empty()
            }
            Expr::ForLoop(f) => {
                self.eval(&f.expr);
                self.walk_block(&f.body);
                Eval::empty()
            }
            Expr::Loop(l) => {
                self.walk_block(&l.body);
                Eval::empty()
            }
            Expr::Return(r) => {
                if let Some(e) = &r.expr {
                    let ev = self.eval(e);
                    self.return_nodes.extend(ev.nodes);
                }
                Eval::empty()
            }
            _ => Eval::empty(),
        }
    }

    fn eval_call(&mut self, c: &ExprCall) -> Eval {
        let line = line_of(c.span());
        let arg_nodes: Vec<FlowNode> = c.args.iter().flat_map(|a| self.eval(a).nodes).collect();
        let Some(callee) = expr_path_string(&c.func) else {
            return Eval::plain(arg_nodes);
        };
        if taint_spec::is_sanitizer(&callee) {
            return Eval::empty();
        }
        if let Some(kind) = taint_spec::source_kind(&callee) {
            let n = self.new_node();
            self.sources.push(TaintSource {
                node: n,
                kind: kind.to_string(),
                line,
            });
            return Eval::plain(vec![n]);
        }
        if let Some(kind) = taint_spec::sink_kind(&callee) {
            if !arg_nodes.is_empty() {
                self.sinks.push(TaintSink {
                    args: arg_nodes.clone(),
                    kind: kind.to_string(),
                    callee: callee.clone(),
                    line,
                });
            }
            return Eval {
                nodes: arg_nodes,
                sink_ctx: Some((kind, callee)),
            };
        }
        // User/unknown function call: record a CallSite + result node so the
        // interprocedural engine can apply the callee's param→sink summary.
        // Keep the conservative intra behavior (args flow to the result) so a
        // tainted arg laundered through the call's return still reaches
        // downstream sinks even without a summary.
        let result = self.new_node();
        for n in &arg_nodes {
            self.edges.push((*n, result));
        }
        self.calls.push(CallSite {
            callee,
            arg_nodes,
            result,
            line,
        });
        Eval::plain(vec![result])
    }

    fn eval_method(&mut self, m: &ExprMethodCall) -> Eval {
        let line = line_of(m.span());
        let recv = self.eval(&m.receiver);
        let method = m.method.to_string();
        let arg_nodes: Vec<FlowNode> = m.args.iter().flat_map(|a| self.eval(a).nodes).collect();

        if taint_spec::is_sanitizer(&method) {
            return Eval::empty();
        }
        if let Some(kind) = taint_spec::source_kind(&method) {
            let n = self.new_node();
            self.sources.push(TaintSource {
                node: n,
                kind: kind.to_string(),
                line,
            });
            return Eval::plain(vec![n]);
        }
        let dotted = format!(".{method}");
        if let Some(kind) = taint_spec::sink_kind(&dotted) {
            if !arg_nodes.is_empty() {
                self.sinks.push(TaintSink {
                    args: arg_nodes.clone(),
                    kind: kind.to_string(),
                    callee: dotted,
                    line,
                });
            }
            let mut nodes = recv.nodes;
            nodes.extend(arg_nodes);
            return Eval {
                nodes,
                sink_ctx: recv.sink_ctx,
            };
        }
        // Builder chain: receiver carries a sink context (e.g. Command::new(..)),
        // so this method (.arg/.args/.env/...) feeds args into that sink.
        if let Some((kind, callee)) = recv.sink_ctx.clone()
            && !arg_nodes.is_empty()
        {
            self.sinks.push(TaintSink {
                args: arg_nodes.clone(),
                kind: kind.to_string(),
                callee,
                line,
            });
        }
        let mut nodes = recv.nodes;
        nodes.extend(arg_nodes);
        Eval {
            nodes,
            sink_ctx: recv.sink_ctx,
        }
    }

    /// Value-producing macros (`format!`, `write!`, `println!`, …) propagate the
    /// taint of their argument expressions. Parse the body as comma-separated
    /// expressions; the format-string literal contributes nothing.
    fn eval_macro(&mut self, mac: &syn::Macro) -> Eval {
        let parsed = mac.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        );
        match parsed {
            Ok(args) => Eval::plain(args.iter().flat_map(|a| self.eval(a).nodes).collect()),
            Err(_) => Eval::empty(),
        }
    }
}

fn line_of(span: Span) -> u32 {
    span.start().line as u32
}

/// Single-segment path → its ident (a bare variable reference).
fn path_single_ident(path: &syn::Path) -> Option<String> {
    if path.segments.len() == 1 {
        Some(path.segments[0].ident.to_string())
    } else {
        None
    }
}

/// Render a path expression as `seg::seg::...` for callee classification.
fn expr_path_string(expr: &Expr) -> Option<String> {
    if let Expr::Path(p) = expr {
        let parts: Vec<String> = p
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("::"))
        }
    } else {
        None
    }
}

/// Bound identifier of a simple `let`/closure pattern (`x`, `mut x`, `ref x`).
fn pat_ident(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(pi) => Some(pi.ident.to_string()),
        Pat::Type(pt) => pat_ident(&pt.pat),
        _ => None,
    }
}

/// Assignment LHS that is a bare variable.
fn expr_path_ident(expr: &Expr) -> Option<String> {
    if let Expr::Path(p) = expr {
        path_single_ident(&p.path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_analysis::taint_dataflow::analyze_function;

    fn findings(src: &str) -> Vec<crate::code_analysis::taint_dataflow::TaintFinding> {
        extract(src).iter().flat_map(analyze_function).collect()
    }

    #[test]
    fn command_injection_via_builder_chain() {
        let src = r#"
            fn run() {
                let user = std::env::var("CMD").unwrap();
                std::process::Command::new("sh").arg("-c").arg(user).output().unwrap();
            }
        "#;
        let f = findings(src);
        assert_eq!(f.len(), 1, "expected one command finding, got {f:?}");
        assert_eq!(f[0].sink_kind, "command");
        assert_eq!(f[0].source_kind, "env");
    }

    #[test]
    fn sql_injection_via_format() {
        let src = r#"
            fn q() {
                let id = std::env::var("ID").unwrap();
                let sql = format!("SELECT * FROM t WHERE id = {}", id);
                sqlx::query(&sql);
            }
        "#;
        let f = findings(src);
        assert_eq!(f.len(), 1, "expected one sql finding, got {f:?}");
        assert_eq!(f[0].sink_kind, "sql");
    }

    #[test]
    fn sanitized_value_is_not_a_finding() {
        let src = r#"
            fn safe() {
                let user = std::env::var("CMD").unwrap();
                let clean = shell_escape(&user);
                std::process::Command::new("sh").arg("-c").arg(clean).output().unwrap();
            }
        "#;
        assert!(findings(src).is_empty(), "sanitized input must not flag");
    }

    #[test]
    fn constant_argument_is_not_a_finding() {
        let src = r#"
            fn fixed() {
                std::process::Command::new("ls").arg("-la").output().unwrap();
            }
        "#;
        assert!(findings(src).is_empty(), "constant args carry no taint");
    }

    #[test]
    fn unrelated_source_and_sink_without_flow() {
        let src = r#"
            fn nope() {
                let _user = std::env::var("X").unwrap();
                std::process::Command::new("ls").arg("-la").output().unwrap();
            }
        "#;
        assert!(
            findings(src).is_empty(),
            "source + sink in same fn but no flow between them"
        );
    }
}
