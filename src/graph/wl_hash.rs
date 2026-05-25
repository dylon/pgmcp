//! Weisfeiler-Lehman graph labeling for structural clone detection
//! (Weisfeiler & Lehman 1968; Shervashidze et al., "Weisfeiler-Lehman Graph
//! Kernels", JMLR 2011). (graph-roadmap Phase 4.6)
//!
//! Iteratively refines each node's label to a hash of (its own label + the
//! sorted multiset of its neighbors' labels). After `h` rounds a node's label
//! encodes the structure of its `h`-hop neighborhood. Initial labels are
//! **degree-based** (identifier-agnostic), so two subgraphs with the same shape
//! get the same labels even when every identifier was renamed — catching
//! structural call-graph clones that the text-level `lsh_clone_detection`
//! misses. Pure + generic over `DiGraph<N, E>`; O(h·(n+e) log Δ).

use std::collections::HashMap;

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};

/// Deterministic 64-bit hash combiner (SplitMix-style finalizer on a fold).
fn mix(h: u64, x: u64) -> u64 {
    let mut z = h
        .wrapping_mul(0x100000001b3)
        .wrapping_add(x)
        .wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// Compute WL labels for every node after `iterations` refinement rounds.
/// Returned vector is indexed by `NodeIndex::index()`. Labels are comparable
/// *within one call* (identical label ⇒ same refined neighborhood structure).
pub fn wl_labels<N, E>(graph: &DiGraph<N, E>, iterations: usize) -> Vec<u64> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    // Initial label = (in_degree, out_degree) packed — purely structural.
    let mut labels: Vec<u64> = graph
        .node_indices()
        .map(|ni| {
            let ind = graph.neighbors_directed(ni, Direction::Incoming).count() as u64;
            let outd = graph.neighbors_directed(ni, Direction::Outgoing).count() as u64;
            mix(mix(0xABCD, ind), outd)
        })
        .collect();

    for _ in 0..iterations {
        let mut next = vec![0u64; n];
        for ni in graph.node_indices() {
            let i = ni.index();
            // Sorted multiset of neighbor labels (undirected) → order-independent.
            let mut nbr: Vec<u64> = graph
                .neighbors_undirected(ni)
                .map(|nb| labels[nb.index()])
                .collect();
            nbr.sort_unstable();
            let mut h = mix(0x1234, labels[i]);
            for x in nbr {
                h = mix(h, x);
            }
            next[i] = h;
        }
        // Canonicalize to small ids so labels don't drift arbitrarily large and
        // collisions across rounds are avoided (stable mapping per round).
        let mut canon: HashMap<u64, u64> = HashMap::new();
        for v in &mut next {
            let next_id = canon.len() as u64;
            *v = *canon.entry(*v).or_insert(next_id);
        }
        labels = next;
    }
    labels
}

/// Group nodes by identical WL label into structural-clone classes (only
/// classes of size ≥ 2 are returned, largest first). Nodes in one class have
/// structurally indistinguishable `iterations`-hop neighborhoods.
pub fn structural_clone_classes<N, E>(
    graph: &DiGraph<N, E>,
    iterations: usize,
) -> Vec<Vec<NodeIndex>> {
    let labels = wl_labels(graph, iterations);
    let mut by_label: HashMap<u64, Vec<NodeIndex>> = HashMap::new();
    for ni in graph.node_indices() {
        by_label.entry(labels[ni.index()]).or_default().push(ni);
    }
    let mut classes: Vec<Vec<NodeIndex>> =
        by_label.into_values().filter(|c| c.len() >= 2).collect();
    classes.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].index().cmp(&b[0].index())));
    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_from(n: usize, edges: &[(usize, usize)]) -> DiGraph<(), ()> {
        let mut g = DiGraph::<(), ()>::new();
        let idx: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
        for &(s, t) in edges {
            g.add_edge(idx[s], idx[t], ());
        }
        g
    }

    #[test]
    fn two_isomorphic_triangles_share_labels() {
        // Triangle {0,1,2} and triangle {3,4,5}: structurally identical, so the
        // six nodes collapse into clone classes spanning both triangles.
        let g = graph_from(6, &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3)]);
        let labels = wl_labels(&g, 2);
        // Every node in a directed 3-cycle has in=out=1 and identical structure.
        assert!(
            labels.iter().all(|&l| l == labels[0]),
            "all 3-cycle nodes are structurally identical: {labels:?}"
        );
        let classes = structural_clone_classes(&g, 2);
        assert_eq!(classes.len(), 1, "one clone class of all six nodes");
        assert_eq!(classes[0].len(), 6);
    }

    #[test]
    fn star_center_differs_from_leaves() {
        // Star: 0 → {1,2,3}. Center has out-deg 3; leaves in-deg 1. Distinct.
        let g = graph_from(4, &[(0, 1), (0, 2), (0, 3)]);
        let labels = wl_labels(&g, 1);
        assert_ne!(labels[0], labels[1], "center vs leaf must differ");
        // The three leaves are mutually structurally identical.
        assert_eq!(labels[1], labels[2]);
        assert_eq!(labels[2], labels[3]);
        let classes = structural_clone_classes(&g, 1);
        assert_eq!(classes.len(), 1, "the 3 leaves form one clone class");
        assert_eq!(classes[0].len(), 3);
    }

    #[test]
    fn empty_graph_safe() {
        let g = graph_from(0, &[]);
        assert!(wl_labels(&g, 3).is_empty());
        assert!(structural_clone_classes(&g, 3).is_empty());
    }
}
