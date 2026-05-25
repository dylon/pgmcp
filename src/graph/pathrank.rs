//! PathRAG-style flow-pruned path enumeration (Chen et al. 2025), generic over
//! any `DiGraph<N, E: EdgeCost>`. (graph-roadmap Phase 3.3)
//!
//! Given seed nodes (the query's dense-similar files), enumerate directed
//! dependency paths following outgoing edges up to `max_hops`, accumulating a
//! *flow* = seed weight × Π edge weights. Paths whose flow drops below
//! `min_flow` are pruned (PathRAG's reliability flow pruning), bounding the
//! search. Paths are ranked by flow (desc), then by length (asc — a shorter
//! route of equal flow is the stronger explanation), then deterministically.
//!
//! Unlike PPR (which ranks *nodes* by relational proximity), this returns the
//! actual *routes* — the import/call chain that connects a query hit to a
//! related file — answering "how does A reach B".

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::types::EdgeCost;

/// A ranked dependency path: the node sequence (seed first), the per-hop edge
/// weights, and the accumulated flow.
#[derive(Debug, Clone)]
pub struct RankedPath {
    pub nodes: Vec<NodeIndex>,
    pub edge_weights: Vec<f64>,
    pub flow: f64,
}

/// Enumerate flow-pruned directed paths from the given seeds.
///
/// - `seeds`: `(node, seed_weight)` — seed weights bias which seeds' paths rank
///   highest (typically the dense query similarity).
/// - `max_hops`: maximum edges per path (clamped to ≥1).
/// - `min_flow`: prune a path once its accumulated flow falls below this.
/// - `k`: cap on returned paths.
///
/// A path of length 0 (the seed alone) is not emitted; every returned path has
/// ≥1 hop. Cycles are prevented by a per-path visited set.
pub fn ranked_paths<N, E: EdgeCost>(
    graph: &DiGraph<N, E>,
    seeds: &[(NodeIndex, f64)],
    max_hops: usize,
    min_flow: f64,
    k: usize,
) -> Vec<RankedPath> {
    let max_hops = max_hops.max(1);
    let mut out: Vec<RankedPath> = Vec::new();

    // Iterative DFS to avoid recursion depth concerns; each stack frame carries
    // the partial path, its visited set, and accumulated flow.
    struct Frame {
        nodes: Vec<NodeIndex>,
        weights: Vec<f64>,
        visited: Vec<NodeIndex>,
        flow: f64,
    }

    for &(seed, w0) in seeds {
        let w0 = if w0.is_finite() && w0 > 0.0 { w0 } else { 1.0 };
        let mut stack = vec![Frame {
            nodes: vec![seed],
            weights: Vec::new(),
            visited: vec![seed],
            flow: w0,
        }];
        while let Some(frame) = stack.pop() {
            let current = *frame.nodes.last().expect("frame has ≥1 node");
            if frame.weights.len() >= max_hops {
                continue;
            }
            for edge in graph.edges_directed(current, Direction::Outgoing) {
                use petgraph::visit::EdgeRef;
                let target = edge.target();
                if frame.visited.contains(&target) {
                    continue; // no cycles within a single path
                }
                let cost = edge.weight().cost();
                // Treat non-positive / non-finite weights as a neutral 1.0 so a
                // missing-weight edge doesn't silently kill every path.
                let cost = if cost.is_finite() && cost > 0.0 {
                    cost
                } else {
                    1.0
                };
                let flow = frame.flow * cost;
                if flow < min_flow {
                    continue; // flow pruning
                }
                let mut nodes = frame.nodes.clone();
                nodes.push(target);
                let mut weights = frame.weights.clone();
                weights.push(cost);
                // Every realized hop is a complete emittable path.
                out.push(RankedPath {
                    nodes: nodes.clone(),
                    edge_weights: weights.clone(),
                    flow,
                });
                if weights.len() < max_hops {
                    let mut visited = frame.visited.clone();
                    visited.push(target);
                    stack.push(Frame {
                        nodes,
                        weights,
                        visited,
                        flow,
                    });
                }
            }
        }
    }

    // Rank: flow desc, then shorter path, then lexicographic by node indices for
    // determinism.
    out.sort_by(|a, b| {
        b.flow
            .partial_cmp(&a.flow)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.nodes.len().cmp(&b.nodes.len()))
            .then_with(|| {
                a.nodes
                    .iter()
                    .map(|n| n.index())
                    .cmp(b.nodes.iter().map(|n| n.index()))
            })
    });
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::{EdgeType, EdgeWeight};

    fn weighted(n: usize, edges: &[(usize, usize, f64)]) -> DiGraph<(), EdgeWeight> {
        let mut g = DiGraph::<(), EdgeWeight>::new();
        let idx: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
        for &(s, t, w) in edges {
            g.add_edge(
                idx[s],
                idx[t],
                EdgeWeight {
                    edge_type: EdgeType::Import,
                    weight: w,
                },
            );
        }
        g
    }

    #[test]
    fn enumerates_and_ranks_by_flow() {
        // 0→1 (0.9) →2 (0.9); 0→3 (0.5). Seed at 0.
        let g = weighted(4, &[(0, 1, 0.9), (1, 2, 0.9), (0, 3, 0.5)]);
        let seeds = [(NodeIndex::new(0), 1.0)];
        let paths = ranked_paths(&g, &seeds, 3, 0.0, 10);
        // Paths: 0→1 (0.9), 0→1→2 (0.81), 0→3 (0.5).
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0].nodes, vec![NodeIndex::new(0), NodeIndex::new(1)]);
        assert!((paths[0].flow - 0.9).abs() < 1e-9);
        // Lowest-flow path is 0→3.
        assert_eq!(
            paths.last().unwrap().nodes,
            vec![NodeIndex::new(0), NodeIndex::new(3)]
        );
    }

    #[test]
    fn flow_pruning_and_hop_cap() {
        let g = weighted(4, &[(0, 1, 0.9), (1, 2, 0.9), (2, 3, 0.9)]);
        let seeds = [(NodeIndex::new(0), 1.0)];
        // min_flow 0.85 prunes anything past the first hop (0.9 ok, 0.81 < 0.85).
        let pruned = ranked_paths(&g, &seeds, 5, 0.85, 10);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].nodes.len(), 2);
        // max_hops 2 caps path length at 2 edges (3 nodes).
        let capped = ranked_paths(&g, &seeds, 2, 0.0, 10);
        assert!(capped.iter().all(|p| p.edge_weights.len() <= 2));
        assert!(capped.iter().any(|p| p.nodes.len() == 3));
    }

    #[test]
    fn cycle_does_not_loop_forever() {
        // 0→1→2→0 cycle. Bounded by the per-path visited set.
        let g = weighted(3, &[(0, 1, 1.0), (1, 2, 1.0), (2, 0, 1.0)]);
        let seeds = [(NodeIndex::new(0), 1.0)];
        let paths = ranked_paths(&g, &seeds, 10, 0.0, 100);
        // Longest simple path from 0 is 0→1→2 (can't revisit 0).
        assert!(paths.iter().all(|p| p.nodes.len() <= 3));
    }
}
