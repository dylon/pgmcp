//! Louvain community detection — extracted from the parent `algorithms.rs`
//! as part of the D.2 god-file split.

use std::collections::HashMap;

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use crate::graph::types::CodeGraph;

/// Result of Louvain community detection.
pub struct LouvainResult {
    /// Node -> community ID mapping.
    pub communities: HashMap<NodeIndex, usize>,
    /// Modularity score Q.
    pub modularity: f64,
    /// Number of communities.
    pub num_communities: usize,
}

/// Louvain community detection on an undirected view of the graph.
/// `resolution`: modularity resolution parameter (1.0 = standard).
pub fn louvain_communities(code_graph: &CodeGraph, resolution: f64) -> LouvainResult {
    let graph = &code_graph.graph;
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let n = nodes.len();

    if n == 0 {
        return LouvainResult {
            communities: HashMap::new(),
            modularity: 0.0,
            num_communities: 0,
        };
    }

    // Build undirected adjacency with summed weights
    // For modularity: m = total edge weight, k_i = sum of edge weights for node i
    let mut adj: HashMap<NodeIndex, HashMap<NodeIndex, f64>> = HashMap::new();
    let mut total_weight = 0.0;

    for edge in graph.edge_references() {
        let (a, b) = (edge.source(), edge.target());
        let w = edge.weight().weight;
        *adj.entry(a).or_default().entry(b).or_insert(0.0) += w;
        *adj.entry(b).or_default().entry(a).or_insert(0.0) += w;
        total_weight += w;
    }

    if total_weight == 0.0 {
        // No edges: each node is its own community
        let communities: HashMap<NodeIndex, usize> =
            nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        return LouvainResult {
            communities,
            modularity: 0.0,
            num_communities: n,
        };
    }

    let m2 = total_weight; // sum of all edge weights (each edge counted once in directed)

    // k_i = sum of edge weights for node i
    let mut k: HashMap<NodeIndex, f64> = HashMap::new();
    for &node in &nodes {
        let sum: f64 = adj
            .get(&node)
            .map(|nbrs| nbrs.values().sum())
            .unwrap_or(0.0);
        k.insert(node, sum);
    }

    // Initialize: each node in its own community
    let mut community: HashMap<NodeIndex, usize> =
        nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let next_community_id = n;

    // Sigma_tot[c] = sum of k_i for all nodes in community c
    let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
    for &node in &nodes {
        let c = community[&node];
        *sigma_tot.entry(c).or_insert(0.0) += k[&node];
    }

    // Phase 1: Local moving
    let mut improved = true;
    let max_passes = 50;
    let mut pass = 0;

    while improved && pass < max_passes {
        improved = false;
        pass += 1;

        for &node in &nodes {
            let node_comm = community[&node];
            let k_i = k[&node];

            // Compute weights to each neighbor community
            let mut comm_weights: HashMap<usize, f64> = HashMap::new();
            if let Some(neighbors) = adj.get(&node) {
                for (&nbr, &w) in neighbors {
                    let nbr_comm = community[&nbr];
                    *comm_weights.entry(nbr_comm).or_insert(0.0) += w;
                }
            }

            // Remove node from its community
            *sigma_tot
                .get_mut(&node_comm)
                .expect("sigma_tot must contain community") -= k_i;

            // Compute delta-Q for each candidate community
            let mut best_comm = node_comm;
            let mut best_delta = 0.0;

            for (&cand_comm, &w_to_comm) in &comm_weights {
                let sigma = *sigma_tot.get(&cand_comm).unwrap_or(&0.0);
                let delta = w_to_comm - resolution * sigma * k_i / m2;
                if delta > best_delta {
                    best_delta = delta;
                    best_comm = cand_comm;
                }
            }

            // Also consider staying in current (now empty of this node) community
            let w_to_own = *comm_weights.get(&node_comm).unwrap_or(&0.0);
            let sigma_own = *sigma_tot.get(&node_comm).unwrap_or(&0.0);
            let delta_own = w_to_own - resolution * sigma_own * k_i / m2;
            if delta_own >= best_delta {
                best_comm = node_comm;
            }

            // Move node to best community
            community.insert(node, best_comm);
            *sigma_tot.entry(best_comm).or_insert(0.0) += k_i;

            if best_comm != node_comm {
                improved = true;
            }
        }
    }

    // Renumber communities to be contiguous
    let mut comm_map: HashMap<usize, usize> = HashMap::new();
    let mut next_id = 0;
    for val in community.values_mut() {
        let new_id = *comm_map.entry(*val).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *val = new_id;
    }
    let _ = next_community_id; // suppress unused warning

    let num_communities = next_id;

    // Compute modularity Q
    let modularity = compute_modularity(&community, &adj, &k, m2, resolution);

    LouvainResult {
        communities: community,
        modularity,
        num_communities,
    }
}

/// Compute modularity Q for a given community assignment.
fn compute_modularity(
    community: &HashMap<NodeIndex, usize>,
    adj: &HashMap<NodeIndex, HashMap<NodeIndex, f64>>,
    k: &HashMap<NodeIndex, f64>,
    m2: f64,
    resolution: f64,
) -> f64 {
    if m2 == 0.0 {
        return 0.0;
    }

    let mut q = 0.0;
    for (&node_i, &comm_i) in community {
        let k_i = k[&node_i];
        if let Some(neighbors) = adj.get(&node_i) {
            for (&node_j, &w_ij) in neighbors {
                if community[&node_j] == comm_i {
                    let k_j = k[&node_j];
                    q += w_ij - resolution * k_i * k_j / m2;
                }
            }
        }
    }
    q / m2
}
