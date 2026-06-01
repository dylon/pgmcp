//! AST→AST LaTeX macro expansion (the renderer's Phase-4 pass).
//!
//! Collects `\newcommand` / `\renewcommand` / `\providecommand` /
//! `\DeclareMathOperator` / `\def` definitions from a parsed `Document` and
//! substitutes their uses, so `\newcommand{\R}{\mathbb{R}}` + `$x\in\R$` renders
//! with `\R` expanded. A pure pass over `latex_parser` nodes — the parser stays
//! untouched, consuming the [`Node::Parameter`] nodes it produces for `#1`..`#9`.
//!
//! **Totality.** Two independent guards make expansion total: a global step
//! budget and a per-expansion cycle set + depth bound. On exceeding either, the
//! macro use is left literal (never hangs, never panics) — its name still
//! renders as a searchable token. Definitions are collected left-to-right
//! (preamble then body) so a use before its definition stays literal, matching
//! TeX's single-pass model.

use std::collections::HashMap;
use std::rc::Rc;

use latex_parser::{
    Argument, Command, Document, Environment, MathContent, Node, NodeRef, Span, Spanned,
};

const MAX_STEPS: u32 = 100_000;
const MAX_DEPTH: u16 = 256;

/// A collected macro definition.
struct MacroDef {
    /// Number of `#1..#n` parameters.
    params: u8,
    /// Replacement body nodes.
    body: Vec<NodeRef>,
}

/// Termination guards shared across the whole expansion.
struct Budget {
    /// Remaining total expansion steps (one per macro use expanded).
    steps: u32,
    /// Current nesting depth of macro-in-macro expansion.
    depth: u16,
    /// Names currently on the expansion stack (direct/mutual cycle detection).
    active: Vec<String>,
}

/// Expand all macros in `doc`, returning a new `Document`.
pub fn expand_macros(doc: &Document) -> Document {
    let mut table: HashMap<String, MacroDef> = HashMap::new();
    let mut budget = Budget {
        steps: MAX_STEPS,
        depth: 0,
        active: Vec::new(),
    };
    let preamble = expand_nodes(&doc.preamble, &mut table, &mut budget);
    let body = doc
        .body
        .as_ref()
        .map(|b| expand_nodes(b, &mut table, &mut budget));
    Document {
        preamble,
        body,
        is_complete: doc.is_complete,
        errors: doc.errors.clone(),
    }
}

fn mk(n: Node, span: Span) -> NodeRef {
    Rc::new(Spanned::new(n, span))
}

/// Expand a node list: register definitions inline (emitting nothing for them),
/// substitute macro uses (grabbing following-sibling arguments), and recurse
/// into every container.
fn expand_nodes(
    nodes: &[NodeRef],
    table: &mut HashMap<String, MacroDef>,
    budget: &mut Budget,
) -> Vec<NodeRef> {
    let mut out = Vec::with_capacity(nodes.len());
    let mut i = 0;
    while i < nodes.len() {
        let n = &nodes[i];
        match &n.node {
            Node::Command(c) => {
                // 1. A definition command: register it, consume any following
                //    siblings it owns (\def), emit nothing.
                if let Some(consumed) = try_register(c, nodes, i, table) {
                    i += 1 + consumed;
                    continue;
                }
                // 2. A use of a known macro: grab `params` following-sibling
                //    arguments, substitute, and recurse the result.
                if let Some(params) = table.get(&c.name).map(|d| d.params) {
                    let (args, consumed) = grab_args(nodes, i + 1, params);
                    let body = table.get(&c.name).expect("present above").body.clone();
                    out.extend(expand_use(&c.name, &body, &args, table, budget));
                    i += 1 + consumed;
                    continue;
                }
                // 3. An ordinary command: recurse into its own arguments.
                out.push(mk(Node::Command(map_command(c, table, budget)), n.span));
                i += 1;
            }
            Node::Group(g) => {
                out.push(mk(Node::Group(expand_nodes(g, table, budget)), n.span));
                i += 1;
            }
            Node::Environment(e) => {
                out.push(mk(
                    Node::Environment(map_environment(e, table, budget)),
                    n.span,
                ));
                i += 1;
            }
            Node::Math(m) => {
                out.push(mk(
                    Node::Math(MathContent {
                        mode: m.mode,
                        content: expand_nodes(&m.content, table, budget),
                        is_closed: m.is_closed,
                    }),
                    n.span,
                ));
                i += 1;
            }
            _ => {
                out.push(Rc::clone(n));
                i += 1;
            }
        }
    }
    out
}

/// Recurse expansion into a command's arguments (a command that is itself not a
/// macro use). The command name and starred-ness are preserved.
fn map_command(c: &Command, table: &mut HashMap<String, MacroDef>, budget: &mut Budget) -> Command {
    Command {
        name: c.name.clone(),
        optional_args: map_args(&c.optional_args, table, budget),
        args: map_args(&c.args, table, budget),
        starred: c.starred,
    }
}

fn map_environment(
    e: &Environment,
    table: &mut HashMap<String, MacroDef>,
    budget: &mut Budget,
) -> Environment {
    Environment {
        name: e.name.clone(),
        args: map_args(&e.args, table, budget),
        body: expand_nodes(&e.body, table, budget),
        is_closed: e.is_closed,
    }
}

fn map_args(
    args: &[Argument],
    table: &mut HashMap<String, MacroDef>,
    budget: &mut Budget,
) -> Vec<Argument> {
    args.iter()
        .map(|a| Argument::new(expand_nodes(&a.content, table, budget), a.kind))
        .collect()
}

/// If `c` is a definition command, register the macro and return the number of
/// *following siblings* it consumed (0 for `\newcommand`-family /
/// `\DeclareMathOperator`, whose arguments are attached; a count for `\def`).
/// Returns `None` if `c` is not a definition.
fn try_register(
    c: &Command,
    nodes: &[NodeRef],
    i: usize,
    table: &mut HashMap<String, MacroDef>,
) -> Option<usize> {
    match c.name.as_str() {
        "newcommand" | "renewcommand" | "providecommand" | "DeclareRobustCommand" => {
            // signature `m o o m`: args = [name, body], optional_args = [n, default].
            let name = control_name(c.args.first()?)?;
            let params = c
                .optional_args
                .first()
                .and_then(|a| arg_text(a).trim().parse::<u8>().ok())
                .unwrap_or(0)
                .min(9);
            let body = c.args.get(1)?.content.clone();
            // `\providecommand` only defines if absent.
            if c.name == "providecommand" && table.contains_key(&name) {
                return Some(0);
            }
            table.insert(name, MacroDef { params, body });
            Some(0)
        }
        "DeclareMathOperator" => {
            // `s m m`: args = [name, operator-text]. The starred form differs only
            // in limit placement, irrelevant to text.
            let name = control_name(c.args.first()?)?;
            let body = c.args.get(1)?.content.clone();
            table.insert(name, MacroDef { params: 0, body });
            Some(0)
        }
        "def" => try_register_def(nodes, i, table),
        _ => None,
    }
}

/// Register a `\def\name<params>{body}`. `\def` parses at arity 0, so the name,
/// any `#k` parameter markers, and the body group are *following siblings*.
/// Returns the number of siblings consumed, or `None` if the shape is not a
/// recognizable `\def`.
fn try_register_def(
    nodes: &[NodeRef],
    i: usize,
    table: &mut HashMap<String, MacroDef>,
) -> Option<usize> {
    // nodes[i] is `\def`. The next node must be the macro-name command.
    let name = match nodes.get(i + 1).map(|n| &n.node) {
        Some(Node::Command(c)) => c.name.clone(),
        _ => return None,
    };
    // Count `#k` parameter siblings up to the body group.
    let mut j = i + 2;
    let mut params: u8 = 0;
    while let Some(n) = nodes.get(j) {
        match &n.node {
            Node::Parameter(_) => {
                params = params.saturating_add(1).min(9);
                j += 1;
            }
            Node::Group(g) => {
                // The body.
                table.insert(
                    name,
                    MacroDef {
                        params,
                        body: g.clone(),
                    },
                );
                return Some(j - i); // siblings consumed after `\def` itself
            }
            _ => return None, // unrecognized delimited-parameter `\def`: skip.
        }
    }
    None
}

/// Grab `n` following-sibling arguments for a macro use starting at `start`.
/// A `Group` contributes its children; any other single node is a one-token
/// argument (TeX's undelimited-argument rule). Returns the bound arguments and
/// the number of siblings consumed.
fn grab_args(nodes: &[NodeRef], start: usize, n: u8) -> (Vec<Vec<NodeRef>>, usize) {
    let mut args = Vec::with_capacity(n as usize);
    let mut j = start;
    for _ in 0..n {
        // Skip whitespace between arguments.
        while matches!(nodes.get(j).map(|x| &x.node), Some(Node::Whitespace(_))) {
            j += 1;
        }
        match nodes.get(j) {
            Some(node) => {
                match &node.node {
                    Node::Group(g) => args.push(g.clone()),
                    _ => args.push(vec![Rc::clone(node)]),
                }
                j += 1;
            }
            None => args.push(Vec::new()), // under-supplied: empty argument.
        }
    }
    (args, j - start)
}

/// Expand one macro use: substitute `#k`, recurse the result, all under the
/// termination guards. On exceeding a guard, leave the use literal.
fn expand_use(
    name: &str,
    body: &[NodeRef],
    args: &[Vec<NodeRef>],
    table: &mut HashMap<String, MacroDef>,
    budget: &mut Budget,
) -> Vec<NodeRef> {
    if budget.steps == 0 || budget.depth >= MAX_DEPTH || budget.active.iter().any(|a| a == name) {
        return leave_literal(name, args);
    }
    budget.steps -= 1;
    budget.active.push(name.to_string());
    budget.depth += 1;
    let substituted = substitute(body, args);
    let expanded = expand_nodes(&substituted, table, budget);
    budget.depth -= 1;
    budget.active.pop();
    expanded
}

/// Re-emit a macro use we declined to expand, without losing its grabbed
/// arguments: the command name followed by each argument as a group.
fn leave_literal(name: &str, args: &[Vec<NodeRef>]) -> Vec<NodeRef> {
    let mut out = Vec::with_capacity(1 + args.len());
    out.push(mk(Node::Command(Command::simple(name)), Span::empty(0)));
    for a in args {
        out.push(mk(Node::Group(a.clone()), Span::empty(0)));
    }
    out
}

/// Substitute `#k` parameter references in a body with the bound arguments.
fn substitute(body: &[NodeRef], args: &[Vec<NodeRef>]) -> Vec<NodeRef> {
    let mut out = Vec::with_capacity(body.len());
    for n in body {
        match &n.node {
            Node::Parameter(k) => {
                let idx = (*k as usize).saturating_sub(1);
                match args.get(idx) {
                    Some(arg) => out.extend(arg.iter().cloned()),
                    None => out.push(Rc::clone(n)), // `#k` with no binding: literal.
                }
            }
            Node::Group(g) => out.push(mk(Node::Group(substitute(g, args)), n.span)),
            Node::Command(c) => out.push(mk(Node::Command(substitute_command(c, args)), n.span)),
            Node::Environment(e) => out.push(mk(
                Node::Environment(substitute_environment(e, args)),
                n.span,
            )),
            Node::Math(m) => out.push(mk(
                Node::Math(MathContent {
                    mode: m.mode,
                    content: substitute(&m.content, args),
                    is_closed: m.is_closed,
                }),
                n.span,
            )),
            _ => out.push(Rc::clone(n)),
        }
    }
    out
}

fn substitute_command(c: &Command, args: &[Vec<NodeRef>]) -> Command {
    Command {
        name: c.name.clone(),
        optional_args: substitute_args(&c.optional_args, args),
        args: substitute_args(&c.args, args),
        starred: c.starred,
    }
}

fn substitute_environment(e: &Environment, args: &[Vec<NodeRef>]) -> Environment {
    Environment {
        name: e.name.clone(),
        args: substitute_args(&e.args, args),
        body: substitute(&e.body, args),
        is_closed: e.is_closed,
    }
}

fn substitute_args(targs: &[Argument], args: &[Vec<NodeRef>]) -> Vec<Argument> {
    targs
        .iter()
        .map(|a| Argument::new(substitute(&a.content, args), a.kind))
        .collect()
}

/// The control-sequence name defined by a `{\name}` argument: the name of the
/// single `Command` node inside the argument's content.
fn control_name(arg: &Argument) -> Option<String> {
    arg.content.iter().find_map(|n| match &n.node {
        Node::Command(c) => Some(c.name.clone()),
        _ => None,
    })
}

/// Flatten an argument's content to text (for parsing a `[n]` parameter count).
fn arg_text(arg: &Argument) -> String {
    let mut s = String::new();
    for n in &arg.content {
        if let Node::Text(t) = &n.node {
            s.push_str(t);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::super::render::{RenderOptions, to_plain_text};
    use super::*;
    use latex_parser::parse;

    fn expand_and_render(src: &str) -> String {
        let doc = parse(src).expect("parse");
        let expanded = expand_macros(&doc);
        to_plain_text(&expanded, src.len(), &RenderOptions::default())
    }

    #[test]
    fn newcommand_no_arg_symbol() {
        let out = expand_and_render(r"\newcommand{\R}{\mathbb{R}} the set $x \in \R$ here");
        assert!(out.contains("the set"), "{out:?}");
        assert!(out.contains("here"), "{out:?}");
        // \R expanded to \mathbb{R} → identifier R (Unicode blackboard not in our
        // table, but the point is the macro use is not dropped/left as a stray).
        assert!(out.contains('R'), "expansion of \\R missing: {out:?}");
    }

    #[test]
    fn newcommand_with_parameters() {
        let out = expand_and_render(r"\newcommand{\sq}[1]{#1 squared} \sq{five}");
        assert!(
            out.contains("five squared"),
            "param substitution failed: {out:?}"
        );
    }

    #[test]
    fn declare_math_operator_emits_text() {
        let out = expand_and_render(r"\DeclareMathOperator{\argmax}{argmax} $\argmax_x f$");
        assert!(out.contains("argmax"), "operator text missing: {out:?}");
    }

    #[test]
    fn def_zero_param() {
        let out = expand_and_render(r"\def\hello{Hello World} \hello!");
        assert!(out.contains("Hello World"), "{out:?}");
    }

    #[test]
    fn recursive_def_terminates() {
        // `\def\a{\a}` then `\a` must terminate (cycle guard), not hang.
        let out = expand_and_render(r"\def\a{\a} start \a end");
        assert!(out.contains("start"), "{out:?}");
        assert!(out.contains("end"), "{out:?}");
    }

    #[test]
    fn mutually_recursive_defs_terminate() {
        let out = expand_and_render(r"\def\a{\b}\def\b{\a} x \a y");
        assert!(out.contains('x'));
        assert!(out.contains('y'));
    }

    #[test]
    fn nested_macro_in_body() {
        let out =
            expand_and_render(r"\newcommand{\inner}{deep}\newcommand{\outer}{\inner\inner} \outer");
        assert!(out.contains("deepdeep"), "nested expansion failed: {out:?}");
    }

    #[test]
    fn undefined_macro_left_as_token() {
        // An unknown macro is not in the table; rendered as its name (searchable).
        let out = expand_and_render(r"\UndefinedMacro here");
        assert!(out.contains("here"), "{out:?}");
    }
}
