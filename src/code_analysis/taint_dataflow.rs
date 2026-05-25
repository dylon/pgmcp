//! Language-agnostic intraprocedural taint reachability (graph-roadmap Phase 2.1).
//!
//! Runs real source→sink reachability over a [`FunctionDataflow`] def-use IR
//! (populated per language by `LanguageBackend::extract_dataflow`). A finding
//! is raised only when a value *derived from* a taint source *reaches* a sink
//! argument along def-use edges **without passing through a sanitizer** — a
//! genuine data-flow check, not the source/sink-in-the-same-file co-occurrence
//! the old regex `taint_analysis` performed (Newsome-Song NDSS 2005; the CPG
//! source→sink reachability framing of Yamaguchi et al. S&P 2014).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::parsing::dataflow::{FlowNode, FunctionDataflow};

/// One realized taint flow: a source whose value reaches a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintFinding {
    pub function: String,
    pub source_kind: String,
    pub source_line: u32,
    pub sink_kind: String,
    pub sink_callee: String,
    pub sink_line: u32,
    /// Witness path of flow-node ids from the source to the tainted sink arg.
    pub path: Vec<FlowNode>,
}

/// Run source→sink taint reachability for one function. At most one finding is
/// emitted per `(source_line, sink_line)` pair (deduped). Sanitized nodes are
/// barriers: taint never propagates out of them.
pub fn analyze_function(df: &FunctionDataflow) -> Vec<TaintFinding> {
    if df.is_trivial() {
        return Vec::new();
    }
    let sanitized: HashSet<FlowNode> = df.sanitized.iter().copied().collect();

    // Forward adjacency over def-use edges.
    let mut adj: HashMap<FlowNode, Vec<FlowNode>> = HashMap::with_capacity(df.flow_edges.len());
    for &(a, b) in &df.flow_edges {
        adj.entry(a).or_default().push(b);
    }

    let mut findings: Vec<TaintFinding> = Vec::new();
    let mut seen_pairs: HashSet<(u32, u32)> = HashSet::new();

    for src in &df.sources {
        // A source node that is itself sanitized originates no taint.
        if sanitized.contains(&src.node) {
            continue;
        }
        // BFS from the source; `prev` reconstructs the witness path. The
        // barrier check on `w` means taint does not flow *through* a sanitizer.
        let mut prev: HashMap<FlowNode, FlowNode> = HashMap::new();
        let mut tainted: HashSet<FlowNode> = HashSet::new();
        tainted.insert(src.node);
        let mut q: VecDeque<FlowNode> = VecDeque::new();
        q.push_back(src.node);
        while let Some(u) = q.pop_front() {
            for &w in adj.get(&u).map(|v| v.as_slice()).unwrap_or(&[]) {
                if sanitized.contains(&w) || tainted.contains(&w) {
                    continue;
                }
                tainted.insert(w);
                prev.insert(w, u);
                q.push_back(w);
            }
        }

        for sink in &df.sinks {
            // The first tainted argument realizes the flow into this sink.
            let Some(&hit) = sink.args.iter().find(|a| tainted.contains(a)) else {
                continue;
            };
            if !seen_pairs.insert((src.line, sink.line)) {
                continue;
            }
            // Reconstruct source → … → sink-arg.
            let mut path = vec![hit];
            let mut cur = hit;
            while cur != src.node {
                match prev.get(&cur) {
                    Some(&p) => {
                        path.push(p);
                        cur = p;
                    }
                    None => break,
                }
            }
            path.reverse();
            findings.push(TaintFinding {
                function: df.function.clone(),
                source_kind: src.kind.clone(),
                source_line: src.line,
                sink_kind: sink.kind.clone(),
                sink_callee: sink.callee.clone(),
                sink_line: sink.line,
                path,
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::dataflow::{TaintSink, TaintSource};

    fn df_with(
        edges: Vec<(FlowNode, FlowNode)>,
        sources: Vec<TaintSource>,
        sanitized: Vec<FlowNode>,
        sinks: Vec<TaintSink>,
    ) -> FunctionDataflow {
        FunctionDataflow {
            function: "f".into(),
            start_line: 1,
            end_line: 10,
            node_count: 8,
            flow_edges: edges,
            sources,
            sanitized,
            sinks,
        }
    }

    fn src(node: FlowNode, line: u32) -> TaintSource {
        TaintSource {
            node,
            kind: "request".into(),
            line,
        }
    }
    fn sink(args: Vec<FlowNode>, line: u32) -> TaintSink {
        TaintSink {
            args,
            kind: "command".into(),
            callee: "Command::new".into(),
            line,
        }
    }

    #[test]
    fn direct_flow_source_to_sink_is_a_finding() {
        // src(0) → 1 → 2 ; sink consumes 2.
        let df = df_with(
            vec![(0, 1), (1, 2)],
            vec![src(0, 2)],
            vec![],
            vec![sink(vec![2], 5)],
        );
        let f = analyze_function(&df);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].source_line, 2);
        assert_eq!(f[0].sink_line, 5);
        assert_eq!(f[0].path, vec![0, 1, 2], "witness path source→…→sink-arg");
    }

    #[test]
    fn sanitizer_on_the_path_blocks_the_finding() {
        // src(0) → 1(sanitized) → 2 ; taint cannot cross 1.
        let df = df_with(
            vec![(0, 1), (1, 2)],
            vec![src(0, 2)],
            vec![1],
            vec![sink(vec![2], 5)],
        );
        assert!(analyze_function(&df).is_empty());
    }

    #[test]
    fn unreachable_sink_is_not_a_finding() {
        // src(0) → 1 ; sink consumes 9 (no path).
        let df = df_with(
            vec![(0, 1)],
            vec![src(0, 2)],
            vec![],
            vec![sink(vec![9], 5)],
        );
        assert!(analyze_function(&df).is_empty());
    }

    #[test]
    fn no_source_or_no_sink_is_trivial() {
        let only_sink = df_with(vec![(0, 1)], vec![], vec![], vec![sink(vec![1], 5)]);
        assert!(analyze_function(&only_sink).is_empty());
        let only_source = df_with(vec![(0, 1)], vec![src(0, 2)], vec![], vec![]);
        assert!(analyze_function(&only_source).is_empty());
    }

    #[test]
    fn one_finding_per_source_sink_pair() {
        // Two disjoint paths from the same source to the same sink line must
        // still yield exactly one finding for that (source,sink) pair.
        let df = df_with(
            vec![(0, 1), (0, 2), (1, 3), (2, 3)],
            vec![src(0, 2)],
            vec![],
            vec![sink(vec![3], 5)],
        );
        assert_eq!(analyze_function(&df).len(), 1);
    }

    #[test]
    fn sanitized_source_originates_no_taint() {
        let df = df_with(
            vec![(0, 1)],
            vec![src(0, 2)],
            vec![0],
            vec![sink(vec![1], 5)],
        );
        assert!(analyze_function(&df).is_empty());
    }
}
