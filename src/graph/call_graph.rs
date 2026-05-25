//! Symbol-resolved call graph.
//!
//! Built from `symbol_references` rows where `ref_kind = 'call'`. Each node is
//! a function (one `file_symbols.id`); each edge is a single resolved call
//! (target_symbol_id present) or an unresolved external call (target_symbol_id
//! NULL, `target_raw` carries the unresolved identifier).
//!
//! Persisted via `code_graph_edges` rows with `edge_type = 'call'` (Phase 1
//! migration in `src/db/migrations.rs`). Existing PageRank / betweenness /
//! community-detection tools that filter on `edge_type` get call-graph
//! variants for free once those rows are populated.

#![allow(dead_code)] // Consumers (taint analysis, dead-code reachability,
// panic-path BFS, feature-envy) land in later phases.

use std::collections::HashMap;
use std::sync::Arc;

use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::types::EdgeCost;
use crate::work_pool::pool::WorkPool;

/// One function node in the call graph.
#[derive(Debug, Clone)]
pub struct FunctionNode {
    pub symbol_id: i64,
    pub file_id: i64,
    pub name: String,
    pub relative_path: String,
    pub language: String,
    /// `true` for methods defined inside `impl`/`class`/`trait` blocks (parent
    /// is a Struct/Class/Trait/Interface symbol).
    pub is_method: bool,
}

/// One edge in the call graph.
#[derive(Debug, Clone, Copy)]
pub struct CallEdge {
    pub weight: f64,
    /// `true` when `target_symbol_id` was Some at construction time. Unresolved
    /// edges (external crates, dynamic dispatch) keep this `false`.
    pub resolved: bool,
}

/// Lets the weighted graph algorithms (Louvain modularity, eigenvector/Katz
/// centrality, Burt constraint) run on the call graph just as they do on the
/// file-level `EdgeWeight` graph.
impl EdgeCost for CallEdge {
    #[inline]
    fn cost(&self) -> f64 {
        self.weight
    }
}

/// One raw edge tuple as read from the database. `source_symbol_id` must be
/// present (enforced by the `cge_call_needs_source_symbol` CHECK); the target
/// pair is optional.
#[derive(Debug, Clone)]
pub struct RawCallEdge {
    pub source_symbol_id: i64,
    pub target_symbol_id: Option<i64>,
    pub target_raw: String,
    pub weight: f64,
}

/// Petgraph wrapper plus a `symbol_id → NodeIndex` lookup table for fast
/// fan_in / fan_out queries keyed on database id.
pub struct CallGraph {
    pub graph: DiGraph<FunctionNode, CallEdge>,
    pub symbol_to_node: HashMap<i64, NodeIndex>,
}

impl CallGraph {
    /// Construct from the materialized node list (one per `file_symbols` of
    /// kind='function') and raw edge tuples. Edges whose source isn't in the
    /// node list are skipped (defensive — should not happen given the CHECK).
    pub fn build(nodes: Vec<FunctionNode>, edges: Vec<RawCallEdge>) -> Self {
        let mut graph: DiGraph<FunctionNode, CallEdge> = DiGraph::new();
        let mut symbol_to_node: HashMap<i64, NodeIndex> = HashMap::with_capacity(nodes.len());
        for node in nodes {
            let id = node.symbol_id;
            let idx = graph.add_node(node);
            symbol_to_node.insert(id, idx);
        }
        for edge in edges {
            let Some(&src) = symbol_to_node.get(&edge.source_symbol_id) else {
                continue;
            };
            // Unresolved edges land on a synthetic sink-per-source so fan_out
            // is correctly counted; we represent unresolved targets by a
            // self-loop with `resolved=false` only if no real target exists.
            // Better: just record the edge with no destination — petgraph
            // requires both endpoints, so unresolved edges are dropped from
            // the in-process graph but the database row remains for raw
            // queries. fan_out is computed below from the raw count.
            if let Some(tgt_id) = edge.target_symbol_id
                && let Some(&dst) = symbol_to_node.get(&tgt_id)
            {
                graph.add_edge(
                    src,
                    dst,
                    CallEdge {
                        weight: edge.weight,
                        resolved: true,
                    },
                );
            }
        }
        CallGraph {
            graph,
            symbol_to_node,
        }
    }

    /// Number of outgoing resolved edges per function (by symbol_id).
    pub fn fan_out_per_function(&self) -> HashMap<i64, u32> {
        let mut out: HashMap<i64, u32> = HashMap::with_capacity(self.graph.node_count());
        for (sym_id, &node_idx) in &self.symbol_to_node {
            let n = self
                .graph
                .neighbors_directed(node_idx, petgraph::Direction::Outgoing)
                .count() as u32;
            out.insert(*sym_id, n);
        }
        out
    }

    /// Number of incoming resolved edges per function (by symbol_id).
    pub fn fan_in_per_function(&self) -> HashMap<i64, u32> {
        let mut out: HashMap<i64, u32> = HashMap::with_capacity(self.graph.node_count());
        for (sym_id, &node_idx) in &self.symbol_to_node {
            let n = self
                .graph
                .neighbors_directed(node_idx, petgraph::Direction::Incoming)
                .count() as u32;
            out.insert(*sym_id, n);
        }
        out
    }

    /// Tarjan strongly-connected components. SCCs of size ≥ 2 indicate mutual
    /// recursion; single-node SCCs with a self-loop indicate direct recursion.
    pub fn sccs(&self) -> Vec<Vec<NodeIndex>> {
        petgraph::algo::tarjan_scc(&self.graph)
    }

    /// Number of nodes (functions) in the graph.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of resolved call edges.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Remap a `NodeIndex`-keyed result onto `symbol_id` — the database id the
    /// rest of the system speaks. Built by inverting `symbol_to_node`; O(n).
    fn remap<V: Copy>(&self, scores: &HashMap<NodeIndex, V>) -> HashMap<i64, V> {
        let mut out: HashMap<i64, V> = HashMap::with_capacity(self.symbol_to_node.len());
        for (&sym_id, &node_idx) in &self.symbol_to_node {
            if let Some(&v) = scores.get(&node_idx) {
                out.insert(sym_id, v);
            }
        }
        out
    }

    /// PageRank over the resolved call graph, keyed by `symbol_id`. Identifies
    /// the load-bearing *functions* of execution (distinct from file PageRank:
    /// a hub file may hold many trivial functions).
    pub fn pagerank(&self, damping: f64, max_iter: usize, tolerance: f64) -> HashMap<i64, f64> {
        let pr = crate::graph::algorithms::pagerank(&self.graph, damping, max_iter, tolerance);
        self.remap(&pr.scores)
    }

    /// Brandes betweenness centrality over the call graph, keyed by `symbol_id`.
    /// Uses the parallel WorkPool path when `work_pool` is supplied (O(V·E), so
    /// callers gate by node count on large graphs).
    pub fn betweenness(&self, work_pool: Option<&Arc<WorkPool>>) -> HashMap<i64, f64> {
        let bc = match work_pool {
            Some(wp) => {
                crate::graph::algorithms::betweenness_centrality_parallel(&self.graph, wp, None)
            }
            None => crate::graph::algorithms::betweenness_centrality(&self.graph),
        };
        self.remap(&bc)
    }

    /// Louvain community assignment over the call graph. Returns
    /// `(symbol_id -> community_id, modularity Q)` — the natural functional
    /// clusters, independent of file/module boundaries.
    pub fn louvain(&self, resolution: f64) -> (HashMap<i64, usize>, f64) {
        let lr = crate::graph::algorithms::louvain_communities(&self.graph, resolution);
        (self.remap(&lr.communities), lr.modularity)
    }

    /// K-core coreness over the call graph, keyed by `symbol_id` — functions in
    /// the densely interconnected execution core (hard to remove/refactor).
    pub fn kcore(&self) -> HashMap<i64, u32> {
        let kc = crate::graph::algorithms_ext::k_core_decomposition(&self.graph);
        self.remap(&kc.coreness)
    }

    /// Harmonic centrality over the call graph, keyed by `symbol_id`.
    pub fn harmonic(&self) -> HashMap<i64, f64> {
        let h = crate::graph::algorithms_ext::harmonic_centrality(&self.graph);
        self.remap(&h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(sym_id: i64, name: &str) -> FunctionNode {
        FunctionNode {
            symbol_id: sym_id,
            file_id: 1,
            name: name.to_string(),
            relative_path: format!("src/{}.rs", name),
            language: "rust".into(),
            is_method: false,
        }
    }

    fn edge(src: i64, tgt: Option<i64>) -> RawCallEdge {
        RawCallEdge {
            source_symbol_id: src,
            target_symbol_id: tgt,
            target_raw: "x".into(),
            weight: 1.0,
        }
    }

    #[test]
    fn build_empty_graph() {
        let g = CallGraph::build(vec![], vec![]);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn build_assigns_node_indices() {
        let g = CallGraph::build(vec![node(1, "a"), node(2, "b")], vec![edge(1, Some(2))]);
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert!(g.symbol_to_node.contains_key(&1));
        assert!(g.symbol_to_node.contains_key(&2));
    }

    #[test]
    fn unresolved_edges_are_skipped() {
        let g = CallGraph::build(vec![node(1, "a"), node(2, "b")], vec![edge(1, None)]);
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn edge_with_unknown_source_skipped() {
        let g = CallGraph::build(vec![node(2, "b")], vec![edge(99, Some(2))]);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn fan_out_counts_resolved_edges() {
        let g = CallGraph::build(
            vec![node(1, "a"), node(2, "b"), node(3, "c")],
            vec![edge(1, Some(2)), edge(1, Some(3))],
        );
        let fo = g.fan_out_per_function();
        assert_eq!(fo.get(&1).copied(), Some(2));
        assert_eq!(fo.get(&2).copied(), Some(0));
    }

    #[test]
    fn fan_in_counts_incoming() {
        let g = CallGraph::build(
            vec![node(1, "a"), node(2, "b"), node(3, "c")],
            vec![edge(1, Some(3)), edge(2, Some(3))],
        );
        let fi = g.fan_in_per_function();
        assert_eq!(fi.get(&3).copied(), Some(2));
        assert_eq!(fi.get(&1).copied(), Some(0));
    }

    #[test]
    fn sccs_detect_mutual_recursion() {
        // a → b → a
        let g = CallGraph::build(
            vec![node(1, "a"), node(2, "b")],
            vec![edge(1, Some(2)), edge(2, Some(1))],
        );
        let sccs = g.sccs();
        assert!(sccs.iter().any(|c| c.len() == 2));
    }

    #[test]
    fn sccs_separate_acyclic_chain() {
        // a → b → c (no cycles)
        let g = CallGraph::build(
            vec![node(1, "a"), node(2, "b"), node(3, "c")],
            vec![edge(1, Some(2)), edge(2, Some(3))],
        );
        let sccs = g.sccs();
        // All SCCs are singletons.
        assert!(sccs.iter().all(|c| c.len() == 1));
        assert_eq!(sccs.len(), 3);
    }

    #[test]
    fn generic_algorithms_run_on_call_graph() {
        // a → b → c → a (3-cycle) plus a → c. Exercises the genericized
        // algorithm library (Phase 1.1) on DiGraph<FunctionNode, CallEdge>.
        let g = CallGraph::build(
            vec![node(1, "a"), node(2, "b"), node(3, "c")],
            vec![
                edge(1, Some(2)),
                edge(2, Some(3)),
                edge(3, Some(1)),
                edge(1, Some(3)),
            ],
        );

        // PageRank: symbol_id-keyed, covers every node, conserves probability.
        let pr = g.pagerank(0.85, 100, 1e-8);
        assert_eq!(pr.len(), 3);
        assert!(pr.contains_key(&1) && pr.contains_key(&2) && pr.contains_key(&3));
        let total: f64 = pr.values().sum();
        assert!((total - 1.0).abs() < 1e-4, "pagerank sum = {}", total);

        // Louvain: every node assigned a community, modularity finite.
        let (comm, q) = g.louvain(1.0);
        assert_eq!(comm.len(), 3);
        assert!(q.is_finite());

        // k-core + harmonic: symbol_id-keyed, cover every node.
        assert_eq!(g.kcore().len(), 3);
        assert_eq!(g.harmonic().len(), 3);

        // Sequential Brandes betweenness: non-negative for every node.
        for (&sym, &v) in &g.betweenness(None) {
            assert!(v >= 0.0, "negative betweenness for symbol {}", sym);
        }
    }
}
