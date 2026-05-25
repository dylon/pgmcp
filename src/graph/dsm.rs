//! Design Structure Matrix (DSM) analysis: propagation cost, visibility
//! fan-in/out, and core-periphery classification. (graph-roadmap Phase 3.2)
//!
//! A DSM is the (files × files) adjacency matrix of the dependency graph; its
//! *transitive closure* (the visibility matrix) is what architecture research
//! actually measures. Rather than materialize the n×n boolean matrix
//! (O(n²) memory), we compute the two summaries that matter — per-node
//! visibility fan-in / fan-out counts and the global propagation cost — from
//! one forward BFS per node (O(n·(n+e)) time, O(n) extra space).
//!
//! References:
//! - MacCormack, Rusnak & Baldwin, "Exploring the Structure of Complex Software
//!   Designs: An Empirical Study of Open Source and Proprietary Code",
//!   Management Science 52(7), 2006 — *propagation cost* (visibility-matrix
//!   density) and the Core / Shared / Control / Peripheral classification by
//!   visibility fan-in (VFI) and fan-out (VFO).
//! - Baldwin, MacCormack & Rusnak, "Hidden Structure: Using Network Methods to
//!   Map System Architecture", 2014 — the largest *cyclic group* (largest SCC)
//!   as the architectural core.
//!
//! Topology-only: generic over any `DiGraph<N, E>`, so it runs unchanged on the
//! file import graph and the function call graph alike (cf. the genericized
//! `algorithms` layer).

use std::collections::{HashSet, VecDeque};

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};

/// Visibility (transitive-closure) summary of a directed dependency graph.
/// Indices align with the graph's `NodeIndex::index()` (0..n).
#[derive(Debug, Clone)]
pub struct DsmAnalysis {
    /// VFO[i] = number of distinct nodes reachable FROM node i (direct +
    /// transitive), excluding i itself. "How much can change here ripple out."
    pub visibility_fan_out: Vec<usize>,
    /// VFI[i] = number of distinct nodes that can reach node i, excluding i.
    /// "How much of the system is exposed to a change in node i."
    pub visibility_fan_in: Vec<usize>,
    /// Propagation cost = density of the visibility matrix =
    /// (Σ reachable ordered pairs) / n². MacCormack's "average fraction of the
    /// system affected by a change to a random element." In [0, 1]; lower is a
    /// more loosely-coupled (better-decoupled) architecture.
    pub propagation_cost: f64,
    /// Node count (length of both visibility vectors).
    pub n: usize,
}

/// Compute the visibility fan-in/out vectors and propagation cost via one
/// forward BFS per node. Self-reachability is excluded throughout.
pub fn analyze_dsm<N, E>(graph: &DiGraph<N, E>) -> DsmAnalysis {
    let n = graph.node_count();
    let mut visibility_fan_out = vec![0usize; n];
    let mut visibility_fan_in = vec![0usize; n];
    if n == 0 {
        return DsmAnalysis {
            visibility_fan_out,
            visibility_fan_in,
            propagation_cost: 0.0,
            n,
        };
    }

    let mut total_pairs: u64 = 0;
    let mut visited = vec![false; n];
    let mut queue: VecDeque<NodeIndex> = VecDeque::new();

    for start in graph.node_indices() {
        // Reset the visited marks (cheaper than reallocating per source).
        for v in visited.iter_mut() {
            *v = false;
        }
        let s_idx = start.index();
        visited[s_idx] = true;
        queue.clear();
        queue.push_back(start);
        let mut reached = 0usize;
        while let Some(u) = queue.pop_front() {
            for nb in graph.neighbors_directed(u, Direction::Outgoing) {
                let ni = nb.index();
                if !visited[ni] {
                    visited[ni] = true;
                    reached += 1;
                    // `start` can reach `nb` ⇒ `nb`'s visibility fan-in grows.
                    visibility_fan_in[ni] += 1;
                    queue.push_back(nb);
                }
            }
        }
        visibility_fan_out[s_idx] = reached;
        total_pairs += reached as u64;
    }

    let propagation_cost = total_pairs as f64 / (n as f64 * n as f64);
    DsmAnalysis {
        visibility_fan_out,
        visibility_fan_in,
        propagation_cost,
        n,
    }
}

/// MacCormack core-periphery class for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePeriphery {
    /// High visibility fan-in AND fan-out: central, both depended-upon and
    /// depending — the architectural backbone (and, when cyclic, the core).
    Core,
    /// High fan-in, low fan-out: depended-upon by many, depends on few — shared
    /// utility / foundation code.
    Shared,
    /// Low fan-in, high fan-out: depends on many, depended-upon by few —
    /// orchestrators, entry points, controllers.
    Control,
    /// Low fan-in and fan-out: leaf / isolated code.
    Peripheral,
}

impl CorePeriphery {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorePeriphery::Core => "core",
            CorePeriphery::Shared => "shared",
            CorePeriphery::Control => "control",
            CorePeriphery::Peripheral => "peripheral",
        }
    }
}

/// Result of the core-periphery classification.
#[derive(Debug, Clone)]
pub struct CorePeripheryResult {
    /// Per-node class, indexed by `NodeIndex::index()`.
    pub classes: Vec<CorePeriphery>,
    /// VFI threshold used for the high/low split (median over all nodes).
    pub vfi_threshold: f64,
    /// VFO threshold used for the high/low split (median over all nodes).
    pub vfo_threshold: f64,
    /// Node indices of the largest *cyclic group* (largest SCC with ≥2 nodes) —
    /// the Baldwin-MacCormack-Rusnak architectural core. Empty for an acyclic
    /// graph.
    pub cyclic_core: Vec<usize>,
}

/// Median of a slice of counts (linear-ish via sort on a copy). Empty ⇒ 0.
fn median(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<usize> = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) as f64 / 2.0
    } else {
        v[mid] as f64
    }
}

/// Classify every node into the MacCormack quadrant using the median VFI / VFO
/// as the high/low thresholds, and separately identify the largest cyclic group
/// (largest SCC) as the architectural core. A node is `Core` when *both* its
/// visibility fan-in and fan-out are at or above the medians.
pub fn classify_core_periphery<N, E>(
    graph: &DiGraph<N, E>,
    dsm: &DsmAnalysis,
) -> CorePeripheryResult {
    let n = dsm.n;
    let vfi_threshold = median(&dsm.visibility_fan_in);
    let vfo_threshold = median(&dsm.visibility_fan_out);

    let mut classes = Vec::with_capacity(n);
    for i in 0..n {
        let hi_in = dsm.visibility_fan_in[i] as f64 >= vfi_threshold;
        let hi_out = dsm.visibility_fan_out[i] as f64 >= vfo_threshold;
        classes.push(match (hi_in, hi_out) {
            (true, true) => CorePeriphery::Core,
            (true, false) => CorePeriphery::Shared,
            (false, true) => CorePeriphery::Control,
            (false, false) => CorePeriphery::Peripheral,
        });
    }

    // Largest cyclic group = largest SCC of size ≥ 2.
    let sccs = petgraph::algo::tarjan_scc(graph);
    let cyclic_core: Vec<usize> = sccs
        .into_iter()
        .filter(|c| c.len() >= 2)
        .max_by_key(|c| c.len())
        .map(|c| {
            let set: HashSet<usize> = c.iter().map(|ix| ix.index()).collect();
            let mut v: Vec<usize> = set.into_iter().collect();
            v.sort_unstable();
            v
        })
        .unwrap_or_default();

    CorePeripheryResult {
        classes,
        vfi_threshold,
        vfo_threshold,
        cyclic_core,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a DiGraph<(), ()> from (src, dst) index pairs over `n` nodes.
    fn graph_from(n: usize, edges: &[(usize, usize)]) -> DiGraph<(), ()> {
        let mut g = DiGraph::<(), ()>::new();
        let idx: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
        for &(s, t) in edges {
            g.add_edge(idx[s], idx[t], ());
        }
        g
    }

    #[test]
    fn linear_chain_propagation_and_visibility() {
        // 0 → 1 → 2 → 3 (a pipeline). Reachable ordered pairs:
        // 0 reaches {1,2,3}=3, 1 reaches {2,3}=2, 2 reaches {3}=1, 3 reaches 0.
        // total = 6 ⇒ propagation_cost = 6 / 16 = 0.375.
        let g = graph_from(4, &[(0, 1), (1, 2), (2, 3)]);
        let dsm = analyze_dsm(&g);
        assert_eq!(dsm.visibility_fan_out, vec![3, 2, 1, 0]);
        // VFI: 0←none=0, 1←{0}=1, 2←{0,1}=2, 3←{0,1,2}=3.
        assert_eq!(dsm.visibility_fan_in, vec![0, 1, 2, 3]);
        assert!((dsm.propagation_cost - 0.375).abs() < 1e-9);
    }

    #[test]
    fn disconnected_graph_has_zero_propagation() {
        let g = graph_from(3, &[]);
        let dsm = analyze_dsm(&g);
        assert_eq!(dsm.propagation_cost, 0.0);
        assert_eq!(dsm.visibility_fan_out, vec![0, 0, 0]);
    }

    #[test]
    fn fully_connected_cycle_has_max_propagation() {
        // 0→1→2→0: every node reaches the other two ⇒ 6/9 pairs.
        let g = graph_from(3, &[(0, 1), (1, 2), (2, 0)]);
        let dsm = analyze_dsm(&g);
        assert_eq!(dsm.visibility_fan_out, vec![2, 2, 2]);
        assert_eq!(dsm.visibility_fan_in, vec![2, 2, 2]);
        assert!((dsm.propagation_cost - (6.0 / 9.0)).abs() < 1e-9);

        // The whole 3-cycle is the cyclic core.
        let cp = classify_core_periphery(&g, &dsm);
        assert_eq!(cp.cyclic_core, vec![0, 1, 2]);
        // All three are symmetric ⇒ all Core (VFI=VFO=2 ≥ medians).
        assert!(cp.classes.iter().all(|c| *c == CorePeriphery::Core));
    }

    #[test]
    fn classifies_shared_and_control_roles() {
        // Hub-and-spoke: 0,1 (controls) → 2 (shared) → 3,4 (leaves).
        //   edges: 0→2, 1→2, 2→3, 2→4
        // VFO: 0={2,3,4}=3, 1={2,3,4}=3, 2={3,4}=2, 3=0, 4=0
        // VFI: 0=0, 1=0, 2={0,1}=2, 3={0,1,2}=3, 4={0,1,2}=3
        let g = graph_from(5, &[(0, 2), (1, 2), (2, 3), (2, 4)]);
        let dsm = analyze_dsm(&g);
        let cp = classify_core_periphery(&g, &dsm);
        // No cycles ⇒ empty cyclic core.
        assert!(cp.cyclic_core.is_empty());
        // Node 2: high VFI and high VFO ⇒ Core (the central hub).
        assert_eq!(cp.classes[2], CorePeriphery::Core);
        // Nodes 3,4: high VFI, zero VFO ⇒ Shared (depended-upon leaves).
        assert_eq!(cp.classes[3], CorePeriphery::Shared);
        assert_eq!(cp.classes[4], CorePeriphery::Shared);
    }
}
