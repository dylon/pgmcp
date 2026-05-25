//! 2-edge connectivity + global min-cut (graph-roadmap Phase 3.6).
//!
//! - **2-edge-connected components** (Tarjan 1972): maximal node sets that stay
//!   connected after removing any single edge — robustly multiply-connected
//!   subsystems. Derived by deleting bridges (Hopcroft-Tarjan, already in
//!   `algorithms_ext::articulation_points_and_bridges`) and taking the connected
//!   components of what remains. A component of size 1 is a single-thread node.
//! - **Global min-cut** (Stoer & Wagner, JACM 1997): the minimum-weight edge set
//!   whose removal splits the graph into two non-empty parts — the natural
//!   "weakest seam", feeding module-decoupling suggestions.
//!
//! Topology / weight generic over `DiGraph<N, E>`; the undirected projection is
//! used (dependency direction doesn't change connectivity). Min-cut is O(V³),
//! so the caller gates it on node count.

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::graph::algorithms_ext::articulation_points_and_bridges;
use crate::graph::types::EdgeCost;

/// Minimal union-find over `0..n` with path compression + union by size.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

#[inline]
fn norm_pair(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Partition the graph's nodes into 2-edge-connected components (bridge
/// removal + union-find over the remaining undirected edges). Components are
/// returned largest-first; every node appears in exactly one (size-1 = a node
/// reachable only across bridges, i.e. a single point of edge failure).
pub fn two_edge_connected_components<N, E>(graph: &DiGraph<N, E>) -> Vec<Vec<NodeIndex>> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    let cut = articulation_points_and_bridges(graph);
    let bridges: HashSet<(usize, usize)> = cut
        .bridges
        .iter()
        .map(|(a, b)| norm_pair(a.index(), b.index()))
        .collect();

    let mut uf = UnionFind::new(n);
    for e in graph.edge_references() {
        let (a, b) = (e.source().index(), e.target().index());
        if a == b {
            continue;
        }
        if !bridges.contains(&norm_pair(a, b)) {
            uf.union(a, b);
        }
    }

    let mut groups: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for ni in graph.node_indices() {
        groups.entry(uf.find(ni.index())).or_default().push(ni);
    }
    let mut comps: Vec<Vec<NodeIndex>> = groups.into_values().collect();
    comps.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].index().cmp(&b[0].index())));
    comps
}

/// A global minimum cut: its total weight and the (smaller-or-equal) side of
/// the bipartition.
#[derive(Debug, Clone)]
pub struct GlobalMinCut {
    pub weight: f64,
    pub partition: Vec<NodeIndex>,
}

/// Stoer-Wagner global minimum cut over the undirected, non-negative-weight
/// projection of `graph`. `None` for graphs with < 2 nodes. O(V³) — gate on
/// node count for large graphs.
pub fn global_min_cut<N, E: EdgeCost>(graph: &DiGraph<N, E>) -> Option<GlobalMinCut> {
    let n = graph.node_count();
    if n < 2 {
        return None;
    }

    // Symmetric weight matrix (sum parallel/opposite edges).
    let mut w = vec![vec![0.0_f64; n]; n];
    for e in graph.edge_references() {
        let (a, b) = (e.source().index(), e.target().index());
        if a == b {
            continue;
        }
        let c = e.weight().cost().max(0.0);
        w[a][b] += c;
        w[b][a] += c;
    }

    // `members[v]` = original vertices merged into the active super-vertex v.
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let mut active: Vec<usize> = (0..n).collect();
    let mut best_weight = f64::INFINITY;
    let mut best_partition: Vec<usize> = Vec::new();

    while active.len() > 1 {
        // Minimum-cut phase: maximum-adjacency ordering from an arbitrary start.
        let start = active[0];
        let mut in_a = vec![false; n];
        in_a[start] = true;
        let mut conn = vec![0.0_f64; n];
        for &v in &active {
            if v != start {
                conn[v] = w[start][v];
            }
        }
        let mut prev = start;
        let mut last = start;
        let mut cut_of_phase = 0.0;
        for _ in 1..active.len() {
            let mut sel = usize::MAX;
            let mut best = f64::NEG_INFINITY;
            for &v in &active {
                if !in_a[v] && conn[v] > best {
                    best = conn[v];
                    sel = v;
                }
            }
            if sel == usize::MAX {
                break;
            }
            in_a[sel] = true;
            prev = last;
            last = sel;
            cut_of_phase = conn[sel];
            for &v in &active {
                if !in_a[v] {
                    conn[v] += w[sel][v];
                }
            }
        }

        if cut_of_phase < best_weight {
            best_weight = cut_of_phase;
            best_partition = members[last].clone();
        }

        // Merge `last` into `prev`.
        let last_members = std::mem::take(&mut members[last]);
        members[prev].extend(last_members);
        for &v in &active {
            if v != last && v != prev {
                w[prev][v] += w[last][v];
                w[v][prev] = w[prev][v];
            }
        }
        active.retain(|&x| x != last);
    }

    if !best_weight.is_finite() {
        return None;
    }
    Some(GlobalMinCut {
        weight: best_weight,
        partition: best_partition.into_iter().map(NodeIndex::new).collect(),
    })
}

/// Refine a community assignment so every community is internally **connected**
/// — the key well-connectedness guarantee of Leiden (Traag, Waltman & van Eck,
/// "From Louvain to Leiden", Sci. Rep. 2019) that plain Louvain lacks (Louvain
/// can emit a community whose induced subgraph is disconnected). Each input
/// community is split into the connected components of its induced undirected
/// subgraph, each getting a fresh id. Already-connected communities are kept
/// whole (renumbered). Returns `(refined_map, num_communities)`.
///
/// This is the additive Leiden refinement the roadmap calls for, applied on top
/// of `louvain_communities`; it is NOT the full iterative local-moving/
/// aggregation Leiden (that is the documented follow-up), but it delivers the
/// cited guarantee — no internally-disconnected community.
pub fn refine_communities_connected<N, E>(
    graph: &DiGraph<N, E>,
    communities: &HashMap<NodeIndex, usize>,
) -> (HashMap<NodeIndex, usize>, usize) {
    let mut by_comm: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for (&ni, &c) in communities {
        by_comm.entry(c).or_default().push(ni);
    }

    let mut refined: HashMap<NodeIndex, usize> = HashMap::with_capacity(communities.len());
    let mut next_id = 0usize;

    // Deterministic ordering: communities ascending, members by node index.
    let mut comms: Vec<usize> = by_comm.keys().copied().collect();
    comms.sort_unstable();
    for c in comms {
        let mut members = by_comm.remove(&c).unwrap_or_default();
        members.sort_by_key(|n| n.index());
        let memberset: HashSet<NodeIndex> = members.iter().copied().collect();
        let mut visited: HashSet<NodeIndex> = HashSet::new();
        for &start in &members {
            if !visited.insert(start) {
                continue;
            }
            let id = next_id;
            next_id += 1;
            refined.insert(start, id);
            let mut q: VecDeque<NodeIndex> = VecDeque::new();
            q.push_back(start);
            while let Some(u) = q.pop_front() {
                // Undirected adjacency: a community is connected regardless of
                // dependency direction.
                for nb in graph.neighbors_undirected(u) {
                    if memberset.contains(&nb) && visited.insert(nb) {
                        refined.insert(nb, id);
                        q.push_back(nb);
                    }
                }
            }
        }
    }
    (refined, next_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::{EdgeType, EdgeWeight};

    fn weighted(n: usize, edges: &[(usize, usize, f64)]) -> DiGraph<(), EdgeWeight> {
        let mut g = DiGraph::<(), EdgeWeight>::new();
        let idx: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
        for &(s, t, wt) in edges {
            g.add_edge(
                idx[s],
                idx[t],
                EdgeWeight {
                    edge_type: EdgeType::Import,
                    weight: wt,
                },
            );
        }
        g
    }

    #[test]
    fn two_triangles_joined_by_a_bridge() {
        // Triangle {0,1,2} — bridge 2-3 — triangle {3,4,5}. The bridge splits the
        // graph into two 2-edge-connected components.
        let g = weighted(
            6,
            &[
                (0, 1, 1.0),
                (1, 2, 1.0),
                (2, 0, 1.0),
                (2, 3, 1.0), // bridge
                (3, 4, 1.0),
                (4, 5, 1.0),
                (5, 3, 1.0),
            ],
        );
        let comps = two_edge_connected_components(&g);
        assert_eq!(
            comps.len(),
            2,
            "two triangles, one bridge → 2 components: {comps:?}"
        );
        assert!(comps.iter().all(|c| c.len() == 3));
    }

    #[test]
    fn min_cut_finds_the_bridge() {
        // Same shape: the global min cut is the single bridge edge (weight 1),
        // separating one triangle from the other.
        let g = weighted(
            6,
            &[
                (0, 1, 5.0),
                (1, 2, 5.0),
                (2, 0, 5.0),
                (2, 3, 1.0), // weakest seam
                (3, 4, 5.0),
                (4, 5, 5.0),
                (5, 3, 5.0),
            ],
        );
        let cut = global_min_cut(&g).expect("≥2 nodes");
        assert!(
            (cut.weight - 1.0).abs() < 1e-9,
            "min cut should be the bridge (1.0), got {}",
            cut.weight
        );
        // One side is exactly one triangle.
        assert_eq!(cut.partition.len(), 3);
    }

    #[test]
    fn single_node_has_no_cut_and_one_component() {
        let g = weighted(1, &[]);
        assert!(global_min_cut(&g).is_none());
        assert_eq!(two_edge_connected_components(&g).len(), 1);
    }

    #[test]
    fn refinement_splits_a_disconnected_community() {
        // 0-1 connected, 2-3 connected, but the two pairs are NOT linked. If a
        // (buggy) community assignment lumps all four together, refinement must
        // split it back into the two connected pieces.
        let g = weighted(4, &[(0, 1, 1.0), (2, 3, 1.0)]);
        let mut comm = HashMap::new();
        for i in 0..4 {
            comm.insert(NodeIndex::new(i), 0usize); // all in community 0
        }
        let (refined, k) = refine_communities_connected(&g, &comm);
        assert_eq!(k, 2, "disconnected community must split into 2");
        // 0 and 1 share a community; 2 and 3 share a different one.
        assert_eq!(refined[&NodeIndex::new(0)], refined[&NodeIndex::new(1)]);
        assert_eq!(refined[&NodeIndex::new(2)], refined[&NodeIndex::new(3)]);
        assert_ne!(refined[&NodeIndex::new(0)], refined[&NodeIndex::new(2)]);
    }

    #[test]
    fn refinement_keeps_a_connected_community_whole() {
        let g = weighted(3, &[(0, 1, 1.0), (1, 2, 1.0)]);
        let mut comm = HashMap::new();
        for i in 0..3 {
            comm.insert(NodeIndex::new(i), 7usize);
        }
        let (refined, k) = refine_communities_connected(&g, &comm);
        assert_eq!(k, 1, "a connected community stays whole");
        let id = refined[&NodeIndex::new(0)];
        assert!(refined.values().all(|&v| v == id));
    }
}
