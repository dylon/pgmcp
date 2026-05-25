//! Interprocedural taint via function summaries (graph-roadmap Phase 3.4).
//!
//! Bounded IFDS-style (Reps, Horwitz & Sagiv, "Precise Interprocedural Dataflow
//! Analysis via Graph Reachability", POPL 1995): each user function gets a
//! param→sink **summary** computed by intraprocedural reachability. A caller
//! then consults a callee's summary at each call site — if a tainted argument
//! lands on a parameter the callee routes to a sink, the
//! `source → … → arg → callee-sink` flow is reported even though it crosses the
//! function boundary, which the intraprocedural engine (Phase 2.1) cannot see.
//!
//! Scope: **within-file** — the function set returned by one
//! `extract_dataflow` call (helpers in the same module, the dominant taint-
//! laundering pattern). Reverse-topological ordering isn't needed for
//! param→sink (a callee's summary is purely intra to the callee), so this is a
//! single pass: summarize every function, then scan each caller's tainted
//! arguments against callee summaries. Cross-file propagation over the
//! materialized call graph is the natural extension.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::parsing::dataflow::{FlowNode, FunctionDataflow};

/// Per-function summary: for each parameter (by positional index), the sink kind
/// its value can reach inside the function, if any.
#[derive(Debug, Clone, Default)]
pub struct FunctionSummary {
    pub param_sink: Vec<Option<String>>,
}

/// An interprocedural taint finding: a source in `caller` flows to an argument
/// that `callee` routes to a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterprocFinding {
    pub caller: String,
    pub source_kind: String,
    pub source_line: u32,
    pub call_line: u32,
    pub callee: String,
    pub param_index: usize,
    pub sink_kind: String,
}

/// Forward adjacency over a function's def-use flow edges.
fn adjacency(df: &FunctionDataflow) -> HashMap<FlowNode, Vec<FlowNode>> {
    let mut adj: HashMap<FlowNode, Vec<FlowNode>> = HashMap::with_capacity(df.flow_edges.len());
    for &(a, b) in &df.flow_edges {
        adj.entry(a).or_default().push(b);
    }
    adj
}

/// Nodes reachable from `start` over the flow edges, not propagating *through* a
/// sanitized node. `start` itself is included unless it is sanitized.
fn reachable(
    start: FlowNode,
    adj: &HashMap<FlowNode, Vec<FlowNode>>,
    sanitized: &HashSet<FlowNode>,
) -> HashSet<FlowNode> {
    let mut seen: HashSet<FlowNode> = HashSet::new();
    if sanitized.contains(&start) {
        return seen;
    }
    seen.insert(start);
    let mut q: VecDeque<FlowNode> = VecDeque::new();
    q.push_back(start);
    while let Some(u) = q.pop_front() {
        for &w in adj.get(&u).map(|v| v.as_slice()).unwrap_or(&[]) {
            if sanitized.contains(&w) || !seen.insert(w) {
                continue;
            }
            q.push_back(w);
        }
    }
    seen
}

/// Compute a function's param→sink summary: for each parameter, does its value
/// reach any sink argument (without crossing a sanitizer)?
pub fn summarize(df: &FunctionDataflow) -> FunctionSummary {
    let mut param_sink = vec![None; df.params.len()];
    if df.params.is_empty() || df.sinks.is_empty() {
        return FunctionSummary { param_sink };
    }
    let sanitized: HashSet<FlowNode> = df.sanitized.iter().copied().collect();
    let adj = adjacency(df);
    // (sink-arg node, sink kind) pairs.
    let sink_args: Vec<(FlowNode, &str)> = df
        .sinks
        .iter()
        .flat_map(|s| s.args.iter().map(move |a| (*a, s.kind.as_str())))
        .collect();

    for (i, &p) in df.params.iter().enumerate() {
        let reach = reachable(p, &adj, &sanitized);
        if reach.is_empty() {
            continue;
        }
        for (arg, kind) in &sink_args {
            if reach.contains(arg) {
                param_sink[i] = Some((*kind).to_string());
                break;
            }
        }
    }
    FunctionSummary { param_sink }
}

/// Analyze a file's functions interprocedurally. Returns findings where a
/// source-tainted argument in some caller reaches a sink inside a called
/// (same-file) function via that callee's summary.
pub fn analyze_file(functions: &[FunctionDataflow]) -> Vec<InterprocFinding> {
    if functions.len() < 2 {
        // A single function has no intra-file callee to summarize against.
        // (Self-recursion is rare and the intra pass already covers in-body sinks.)
        return Vec::new();
    }
    // 1. Summarize every function by name. On duplicate names (overloads in
    //    different impls), keep the one with the most informative summary.
    let mut summaries: HashMap<&str, FunctionSummary> = HashMap::with_capacity(functions.len());
    for f in functions {
        let s = summarize(f);
        let informative = s.param_sink.iter().filter(|x| x.is_some()).count();
        match summaries.get(f.function.as_str()) {
            Some(prev) if prev.param_sink.iter().filter(|x| x.is_some()).count() >= informative => {
            }
            _ => {
                summaries.insert(f.function.as_str(), s);
            }
        }
    }

    let mut findings: Vec<InterprocFinding> = Vec::new();
    let mut seen: HashSet<(String, u32, usize)> = HashSet::new();

    // 2. For each caller, taint from its sources, then test call-site args
    //    against callee summaries.
    for caller in functions {
        if caller.sources.is_empty() || caller.calls.is_empty() {
            continue;
        }
        let sanitized: HashSet<FlowNode> = caller.sanitized.iter().copied().collect();
        let adj = adjacency(caller);

        // Union of nodes reachable from each source; remember the originating
        // source (kind, line) of each tainted node for the witness.
        let mut tainted: HashSet<FlowNode> = HashSet::new();
        let mut origin: HashMap<FlowNode, (String, u32)> = HashMap::new();
        for s in &caller.sources {
            for n in reachable(s.node, &adj, &sanitized) {
                if tainted.insert(n) {
                    origin.insert(n, (s.kind.clone(), s.line));
                }
            }
        }
        if tainted.is_empty() {
            continue;
        }

        for call in &caller.calls {
            let Some(summary) = summaries.get(call.callee.as_str()) else {
                continue;
            };
            for (i, &arg) in call.arg_nodes.iter().enumerate() {
                if !tainted.contains(&arg) {
                    continue;
                }
                let Some(Some(sink_kind)) = summary.param_sink.get(i) else {
                    continue;
                };
                let key = (caller.function.clone(), call.line, i);
                if !seen.insert(key) {
                    continue;
                }
                let (source_kind, source_line) = origin.get(&arg).cloned().unwrap_or_default();
                findings.push(InterprocFinding {
                    caller: caller.function.clone(),
                    source_kind,
                    source_line,
                    call_line: call.line,
                    callee: call.callee.clone(),
                    param_index: i,
                    sink_kind: sink_kind.clone(),
                });
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::rust::rust_dataflow::extract;

    fn interproc(src: &str) -> Vec<InterprocFinding> {
        analyze_file(&extract(src))
    }

    #[test]
    fn taint_laundered_through_helper_is_caught() {
        // `outer` reads env (source) and passes it to `helper`, which sinks it
        // into a Command — a flow the intraprocedural engine can't see.
        let src = r#"
            fn outer() {
                let u = std::env::var("CMD").unwrap();
                helper(u);
            }
            fn helper(c: String) {
                std::process::Command::new("sh").arg("-c").arg(c).output().unwrap();
            }
        "#;
        let f = interproc(src);
        assert_eq!(
            f.len(),
            1,
            "expected one interprocedural finding, got {f:?}"
        );
        assert_eq!(f[0].callee, "helper");
        assert_eq!(f[0].param_index, 0);
        assert_eq!(f[0].sink_kind, "command");
        assert_eq!(f[0].source_kind, "env");
    }

    #[test]
    fn untainted_argument_to_sink_helper_is_not_flagged() {
        // helper sinks its param, but `outer` passes a constant — no flow.
        let src = r#"
            fn outer() {
                helper("ls".to_string());
            }
            fn helper(c: String) {
                std::process::Command::new("sh").arg("-c").arg(c).output().unwrap();
            }
        "#;
        assert!(
            interproc(src).is_empty(),
            "constant argument must not flag interprocedurally"
        );
    }

    #[test]
    fn helper_that_does_not_sink_param_is_safe() {
        // helper takes the tainted value but never routes it to a sink.
        let src = r#"
            fn outer() {
                let u = std::env::var("CMD").unwrap();
                helper(u);
            }
            fn helper(c: String) {
                println!("{}", c.len());
            }
        "#;
        assert!(
            interproc(src).is_empty(),
            "a helper that doesn't sink its param yields no finding"
        );
    }

    #[test]
    fn summarize_marks_param_that_reaches_sink() {
        let src = r#"
            fn helper(c: String) {
                std::process::Command::new("sh").arg("-c").arg(c).output().unwrap();
            }
        "#;
        let fns = extract(src);
        let helper = fns
            .iter()
            .find(|f| f.function == "helper")
            .expect("helper kept");
        let s = summarize(helper);
        assert_eq!(s.param_sink.first(), Some(&Some("command".to_string())));
    }
}
