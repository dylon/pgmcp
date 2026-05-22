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

use petgraph::graph::{DiGraph, NodeIndex};

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
}
