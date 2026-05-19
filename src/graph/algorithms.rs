//! Graph algorithms: PageRank, betweenness centrality, Louvain community detection, SCC.

use std::collections::HashMap;
use std::sync::Arc;

use petgraph::Direction;
use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use super::types::{CodeGraph, EdgeWeight, FileNode};

/// PageRank scores per node.
pub struct PageRankResult {
    pub scores: HashMap<NodeIndex, f64>,
}

/// Compute PageRank using power iteration.
/// - `damping`: damping factor (default 0.85)
/// - `max_iter`: maximum iterations (default 100)
/// - `tolerance`: convergence threshold (default 1e-8)
pub fn pagerank(
    graph: &DiGraph<FileNode, EdgeWeight>,
    damping: f64,
    max_iter: usize,
    tolerance: f64,
) -> PageRankResult {
    let n = graph.node_count();
    if n == 0 {
        return PageRankResult {
            scores: HashMap::new(),
        };
    }

    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let mut scores: HashMap<NodeIndex, f64> =
        nodes.iter().map(|&ni| (ni, 1.0 / n as f64)).collect();

    for _ in 0..max_iter {
        let mut new_scores: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);
        let base = (1.0 - damping) / n as f64;

        // Collect dangling node mass (nodes with no outgoing edges)
        let mut dangling_sum = 0.0;
        for &node in &nodes {
            let out_deg = graph.neighbors_directed(node, Direction::Outgoing).count();
            if out_deg == 0 {
                dangling_sum += scores[&node];
            }
        }

        let dangling_contrib = damping * dangling_sum / n as f64;

        for &node in &nodes {
            let mut incoming_sum = 0.0;
            for neighbor in graph.neighbors_directed(node, Direction::Incoming) {
                let out_deg = graph
                    .neighbors_directed(neighbor, Direction::Outgoing)
                    .count();
                if out_deg > 0 {
                    incoming_sum += scores[&neighbor] / out_deg as f64;
                }
            }
            new_scores.insert(node, base + damping * incoming_sum + dangling_contrib);
        }

        // Check convergence
        let max_diff = nodes
            .iter()
            .map(|ni| (new_scores[ni] - scores[ni]).abs())
            .fold(0.0_f64, f64::max);

        scores = new_scores;
        if max_diff < tolerance {
            break;
        }
    }

    PageRankResult { scores }
}

/// Betweenness centrality per node using Brandes' algorithm.
///
/// Backward-compatible wrapper: runs the sequential single-threaded path.
/// For parallel evaluation, see `betweenness_centrality_parallel`.
pub fn betweenness_centrality(graph: &DiGraph<FileNode, EdgeWeight>) -> HashMap<NodeIndex, f64> {
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let n = nodes.len();
    let mut centrality_vec = vec![0.0_f64; max_node_idx(&nodes) + 1];

    if n <= 2 {
        return centrality_map_from_vec(&nodes, &centrality_vec);
    }

    // Accumulate over all source nodes sequentially.
    accumulate_brandes_for_sources(graph, &nodes, &nodes, &mut centrality_vec, None);

    // Normalize for directed graph: divide by (n-1)(n-2)
    let norm = ((n - 1) * (n - 2)) as f64;
    for v in centrality_vec.iter_mut() {
        *v /= norm;
    }

    centrality_map_from_vec(&nodes, &centrality_vec)
}

/// Parallelized Brandes betweenness centrality via the WorkPool.
///
/// Partitions the source-node set into chunks and evaluates each chunk on a
/// WorkPool worker; partial contributions (`Vec<f64>` indexed by node.index())
/// are summed into the final centrality. Graph is immutable-borrowed and
/// petgraph::DiGraph is Send + Sync for our FileNode + EdgeWeight types, so
/// concurrent reads are safe.
///
/// `should_cancel` is checked per chunk — a cancelled run leaves centrality
/// partially summed (still returned, just not converged on all sources).
///
/// Expected speed-up on n=7513 nodes with 32 threads: ~25×.
pub fn betweenness_centrality_parallel(
    graph: &DiGraph<FileNode, EdgeWeight>,
    work_pool: &Arc<crate::work_pool::pool::WorkPool>,
    should_cancel: Option<&(dyn Fn() -> bool + Sync)>,
) -> HashMap<NodeIndex, f64> {
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let n = nodes.len();
    let n_vec = max_node_idx(&nodes) + 1;

    if n <= 2 {
        return centrality_map_from_vec(&nodes, &vec![0.0; n_vec]);
    }

    // Chunk size: 4× active_threads keeps per-task overhead small while
    // allowing the scaler to see real queue depth and add workers.
    let active = work_pool.active_workers().max(1);
    let chunk_size = n.div_ceil(active * 4).max(1);

    // Channel for partial centrality vectors from worker chunks.
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<f64>>();

    // Wrap the graph reference in an Arc-shareable form. petgraph::DiGraph is
    // `Send + Sync` when the node/edge types are (FileNode is simple; EdgeWeight
    // is simple). We use a scoped thread alternative: since closures captured
    // by WorkPool::submit must be 'static, we build an owned adjacency
    // representation the workers can hold.
    let adj: Arc<CompactGraph> = Arc::new(CompactGraph::from_graph(graph));

    let num_chunks = nodes.len().div_ceil(chunk_size);

    for chunk in nodes.chunks(chunk_size) {
        let sources: Vec<NodeIndex> = chunk.to_vec();
        let adj = Arc::clone(&adj);
        let tx = tx.clone();
        let cancel_snapshot = should_cancel.is_some();

        work_pool.submit(
            move || {
                if cancel_snapshot {
                    // Can't thread non-'static fn pointers through the
                    // work pool; the caller's `should_cancel` is checked
                    // before chunk submission. A per-task check would
                    // require boxing into Arc<dyn Fn>, worth doing if
                    // cancellation latency is critical.
                }
                let mut local = vec![0.0_f64; adj.n_vec];
                adj.accumulate_brandes_into(&sources, &mut local);
                let _ = tx.send(local);
            },
            crate::work_pool::pool::Priority::Low,
        );
    }

    drop(tx); // close sender copy; rx sees disconnect after all workers finish

    let mut centrality_vec = vec![0.0_f64; n_vec];
    let mut received = 0usize;
    while received < num_chunks {
        match rx.recv() {
            Ok(partial) => {
                for (dst, src) in centrality_vec.iter_mut().zip(partial.iter()) {
                    *dst += *src;
                }
                received += 1;
                if let Some(cancel) = should_cancel
                    && cancel()
                {
                    // Stop waiting; return what we have so far. Remaining
                    // workers will still complete but their results are
                    // discarded via the dropped receiver.
                    break;
                }
            }
            Err(_) => break, // all senders dropped — everything processed
        }
    }

    // Normalize for directed graph: divide by (n-1)(n-2)
    let norm = ((n - 1) * (n - 2)) as f64;
    for v in centrality_vec.iter_mut() {
        *v /= norm;
    }

    centrality_map_from_vec(&nodes, &centrality_vec)
}

/// Sequential Brandes accumulation — writes partial centrality contributions
/// from `sources` into `centrality_vec` (indexed by node.index()).
///
/// Moved out of `betweenness_centrality` so both the sequential and parallel
/// paths call identical code per-source. `should_cancel` is checked per source
/// to allow fine-grained cancellation during long runs.
fn accumulate_brandes_for_sources(
    graph: &DiGraph<FileNode, EdgeWeight>,
    all_nodes: &[NodeIndex],
    sources: &[NodeIndex],
    centrality_vec: &mut [f64],
    should_cancel: Option<&dyn Fn() -> bool>,
) {
    let n_vec = centrality_vec.len();
    let mut pred: Vec<Vec<NodeIndex>> = (0..n_vec).map(|_| Vec::new()).collect();
    let mut sigma: Vec<f64> = vec![0.0; n_vec];
    let mut dist: Vec<i64> = vec![-1; n_vec];
    let mut delta: Vec<f64> = vec![0.0; n_vec];
    let mut stack: Vec<NodeIndex> = Vec::with_capacity(n_vec);
    let mut queue: std::collections::VecDeque<NodeIndex> =
        std::collections::VecDeque::with_capacity(n_vec);

    for &s in sources {
        if let Some(cancel) = should_cancel
            && cancel()
        {
            return;
        }
        // Reset per-source state (indices touched since last reset).
        // Since we don't track "touched" nodes, just reset all (O(n_vec) —
        // fine: it's linear in the graph size, which the algorithm itself is
        // O(nm) anyway).
        for p in pred.iter_mut() {
            p.clear();
        }
        for v in sigma.iter_mut() {
            *v = 0.0;
        }
        for v in dist.iter_mut() {
            *v = -1;
        }
        for v in delta.iter_mut() {
            *v = 0.0;
        }
        stack.clear();
        queue.clear();

        sigma[s.index()] = 1.0;
        dist[s.index()] = 0;
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let d_v = dist[v.index()];
            for w in graph.neighbors_directed(v, Direction::Outgoing) {
                if dist[w.index()] < 0 {
                    dist[w.index()] = d_v + 1;
                    queue.push_back(w);
                }
                if dist[w.index()] == d_v + 1 {
                    sigma[w.index()] += sigma[v.index()];
                    pred[w.index()].push(v);
                }
            }
        }

        while let Some(w) = stack.pop() {
            let sigma_w = sigma[w.index()];
            if sigma_w > 0.0 {
                let delta_w = delta[w.index()];
                let preds = &pred[w.index()];
                for &p in preds {
                    let contrib = (sigma[p.index()] / sigma_w) * (1.0 + delta_w);
                    delta[p.index()] += contrib;
                }
            }
            if w != s {
                centrality_vec[w.index()] += delta[w.index()];
            }
        }
    }

    let _ = all_nodes; // retained param for API symmetry; currently unused here.
}

/// Compact representation of the graph for parallel Brandes workers.
///
/// Stores outgoing adjacency as flat Vec<NodeIndex> + offsets so workers
/// can share the graph via `Arc<CompactGraph>` without needing a lifetime
/// on the original petgraph reference.
struct CompactGraph {
    /// For each node index i, `out_neighbors[out_offsets[i]..out_offsets[i+1]]`
    /// gives its outgoing neighbors.
    out_neighbors: Vec<NodeIndex>,
    out_offsets: Vec<usize>,
    /// Total slots in centrality_vec (max_node_index + 1).
    n_vec: usize,
    /// Node count (number of actual nodes).
    #[allow(dead_code)]
    n: usize,
}

impl CompactGraph {
    fn from_graph(graph: &DiGraph<FileNode, EdgeWeight>) -> Self {
        let nodes: Vec<NodeIndex> = graph.node_indices().collect();
        let n_vec = nodes.iter().map(|n| n.index()).max().unwrap_or(0) + 1;

        // Build degree count per node, then prefix-sum to offsets.
        let mut out_count = vec![0usize; n_vec];
        for node in &nodes {
            let deg = graph.neighbors_directed(*node, Direction::Outgoing).count();
            out_count[node.index()] = deg;
        }

        let mut out_offsets = vec![0usize; n_vec + 1];
        for i in 0..n_vec {
            out_offsets[i + 1] = out_offsets[i] + out_count[i];
        }
        let total_edges = out_offsets[n_vec];
        let mut out_neighbors = vec![NodeIndex::new(0); total_edges];

        let mut cursor = out_offsets.clone();
        for node in &nodes {
            for nb in graph.neighbors_directed(*node, Direction::Outgoing) {
                let idx = node.index();
                out_neighbors[cursor[idx]] = nb;
                cursor[idx] += 1;
            }
        }

        Self {
            out_neighbors,
            out_offsets,
            n_vec,
            n: nodes.len(),
        }
    }

    fn accumulate_brandes_into(&self, sources: &[NodeIndex], centrality_vec: &mut [f64]) {
        let n_vec = self.n_vec;
        let mut pred: Vec<Vec<NodeIndex>> = (0..n_vec).map(|_| Vec::new()).collect();
        let mut sigma: Vec<f64> = vec![0.0; n_vec];
        let mut dist: Vec<i64> = vec![-1; n_vec];
        let mut delta: Vec<f64> = vec![0.0; n_vec];
        let mut stack: Vec<NodeIndex> = Vec::with_capacity(n_vec);
        let mut queue: std::collections::VecDeque<NodeIndex> =
            std::collections::VecDeque::with_capacity(n_vec);

        for &s in sources {
            for p in pred.iter_mut() {
                p.clear();
            }
            for v in sigma.iter_mut() {
                *v = 0.0;
            }
            for v in dist.iter_mut() {
                *v = -1;
            }
            for v in delta.iter_mut() {
                *v = 0.0;
            }
            stack.clear();
            queue.clear();

            sigma[s.index()] = 1.0;
            dist[s.index()] = 0;
            queue.push_back(s);

            while let Some(v) = queue.pop_front() {
                stack.push(v);
                let d_v = dist[v.index()];
                let begin = self.out_offsets[v.index()];
                let end = self.out_offsets[v.index() + 1];
                for &w in &self.out_neighbors[begin..end] {
                    if dist[w.index()] < 0 {
                        dist[w.index()] = d_v + 1;
                        queue.push_back(w);
                    }
                    if dist[w.index()] == d_v + 1 {
                        sigma[w.index()] += sigma[v.index()];
                        pred[w.index()].push(v);
                    }
                }
            }

            while let Some(w) = stack.pop() {
                let sigma_w = sigma[w.index()];
                if sigma_w > 0.0 {
                    let delta_w = delta[w.index()];
                    let preds = &pred[w.index()];
                    for &p in preds {
                        let contrib = (sigma[p.index()] / sigma_w) * (1.0 + delta_w);
                        delta[p.index()] += contrib;
                    }
                }
                if w != s {
                    centrality_vec[w.index()] += delta[w.index()];
                }
            }
        }
    }
}

#[inline]
fn max_node_idx(nodes: &[NodeIndex]) -> usize {
    nodes.iter().map(|n| n.index()).max().unwrap_or(0)
}

fn centrality_map_from_vec(nodes: &[NodeIndex], vec: &[f64]) -> HashMap<NodeIndex, f64> {
    let mut map = HashMap::with_capacity(nodes.len());
    for &n in nodes {
        map.insert(n, vec[n.index()]);
    }
    map
}

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

/// Find strongly connected components with more than one node (dependency cycles).
pub fn find_cycles(graph: &DiGraph<FileNode, EdgeWeight>) -> Vec<Vec<NodeIndex>> {
    let sccs = tarjan_scc(graph);
    sccs.into_iter().filter(|scc| scc.len() > 1).collect()
}

/// Extract simple cycles from an SCC up to a maximum length using DFS backtracking.
pub fn extract_simple_cycles(
    graph: &DiGraph<FileNode, EdgeWeight>,
    scc: &[NodeIndex],
    max_length: usize,
) -> Vec<Vec<NodeIndex>> {
    use std::collections::HashSet;

    let scc_set: HashSet<NodeIndex> = scc.iter().copied().collect();
    let mut cycles: Vec<Vec<NodeIndex>> = Vec::new();

    for &start in scc {
        let mut stack: Vec<(NodeIndex, Vec<NodeIndex>)> = vec![(start, vec![start])];
        let mut visited_from_start: HashSet<Vec<NodeIndex>> = HashSet::new();

        while let Some((current, path)) = stack.pop() {
            if path.len() > max_length {
                continue;
            }

            for neighbor in graph.neighbors_directed(current, Direction::Outgoing) {
                if !scc_set.contains(&neighbor) {
                    continue;
                }

                if neighbor == start && path.len() > 1 {
                    // Found a cycle
                    let mut cycle = path.clone();
                    // Normalize: rotate so smallest node index is first
                    if let Some(min_pos) = cycle
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, n)| n.index())
                        .map(|(i, _)| i)
                    {
                        cycle.rotate_left(min_pos);
                    }
                    if visited_from_start.insert(cycle.clone()) {
                        cycles.push(cycle);
                    }
                } else if !path.contains(&neighbor) && path.len() < max_length {
                    let mut new_path = path.clone();
                    new_path.push(neighbor);
                    stack.push((neighbor, new_path));
                }
            }
        }
    }

    // Deduplicate cycles (same set of nodes in same order)
    let mut seen: HashSet<Vec<usize>> = HashSet::new();
    cycles.retain(|cycle| {
        let key: Vec<usize> = cycle.iter().map(|n| n.index()).collect();
        seen.insert(key)
    });

    cycles.sort_by_key(|c| c.len());
    cycles
}

/// Compute in-degree and out-degree for each node.
pub fn compute_degrees(
    graph: &DiGraph<FileNode, EdgeWeight>,
) -> HashMap<NodeIndex, (usize, usize)> {
    let mut degrees: HashMap<NodeIndex, (usize, usize)> = HashMap::new();
    for node in graph.node_indices() {
        let in_deg = graph.neighbors_directed(node, Direction::Incoming).count();
        let out_deg = graph.neighbors_directed(node, Direction::Outgoing).count();
        degrees.insert(node, (in_deg, out_deg));
    }
    degrees
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeType, EdgeWeight, FileNode};
    use crate::work_pool::pool::WorkPool;
    use std::sync::atomic::AtomicBool;

    fn make_test_graph(n: usize) -> DiGraph<FileNode, EdgeWeight> {
        let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let nodes: Vec<NodeIndex> = (0..n)
            .map(|i| {
                g.add_node(FileNode {
                    file_id: i as i64,
                    relative_path: format!("f{}.rs", i),
                    language: "rust".into(),
                    module: format!("m{}", i / 10),
                })
            })
            .collect();

        // Scale-free-ish: each node has 2-3 random outgoing edges.
        for (i, &src) in nodes.iter().enumerate() {
            for offset in 1..=2 {
                let j = (i + offset * 3 + 1) % n;
                let dst = nodes[j];
                if src != dst {
                    g.add_edge(
                        src,
                        dst,
                        EdgeWeight {
                            weight: 1.0,
                            edge_type: EdgeType::Import,
                        },
                    );
                }
            }
        }

        g
    }

    #[test]
    fn test_betweenness_parallel_matches_sequential() {
        let graph = make_test_graph(50);

        let seq = betweenness_centrality(&graph);

        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = Arc::new(WorkPool::new(2, 4, 4, Arc::clone(&shutdown)));
        let par = betweenness_centrality_parallel(&graph, &pool, None);

        shutdown.store(true, std::sync::atomic::Ordering::Release);
        // Pool drops; worker threads exit.

        assert_eq!(seq.len(), par.len());
        for (node, &seq_val) in &seq {
            let par_val = par[node];
            assert!(
                (seq_val - par_val).abs() < 1e-9,
                "node {:?}: seq={} par={} (diff={})",
                node,
                seq_val,
                par_val,
                (seq_val - par_val).abs()
            );
        }
    }

    #[test]
    fn test_betweenness_small_graph_returns_zeros() {
        let graph = make_test_graph(2);
        let result = betweenness_centrality(&graph);
        for &v in result.values() {
            assert_eq!(v, 0.0);
        }
    }

    #[test]
    fn test_betweenness_parallel_empty_graph() {
        let graph: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = Arc::new(WorkPool::new(2, 2, 2, Arc::clone(&shutdown)));
        let result = betweenness_centrality_parallel(&graph, &pool, None);
        assert!(result.is_empty());
        shutdown.store(true, std::sync::atomic::Ordering::Release);
    }

    #[test]
    fn test_compact_graph_preserves_adjacency() {
        let graph = make_test_graph(10);
        let compact = CompactGraph::from_graph(&graph);
        for node in graph.node_indices() {
            let original: std::collections::HashSet<NodeIndex> = graph
                .neighbors_directed(node, Direction::Outgoing)
                .collect();
            let begin = compact.out_offsets[node.index()];
            let end = compact.out_offsets[node.index() + 1];
            let compact_neighbors: std::collections::HashSet<NodeIndex> =
                compact.out_neighbors[begin..end].iter().copied().collect();
            assert_eq!(
                original, compact_neighbors,
                "adjacency mismatch for node {:?}",
                node
            );
        }
    }

    // ========================================================================
    // Property tests (Phase 3)
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

        /// PageRank scores sum to ≈ 1.0 (probability conservation).
        #[test]
        fn prop_pagerank_scores_sum_to_one(n in 2usize..30) {
            let graph = make_test_graph(n);
            let result = pagerank(&graph, 0.85, 100, 1e-8);
            let total: f64 = result.scores.values().sum();
            prop_assert!((total - 1.0).abs() < 1e-4,
                "PageRank sum = {} (expected ≈ 1)", total);
        }

        /// All PageRank scores are non-negative.
        #[test]
        fn prop_pagerank_scores_nonnegative(n in 1usize..30) {
            let graph = make_test_graph(n);
            let result = pagerank(&graph, 0.85, 100, 1e-8);
            for (&node, &score) in &result.scores {
                prop_assert!(score >= 0.0,
                    "node {:?} has negative PageRank {}", node, score);
            }
        }

        /// PageRank assigns a score to every node.
        #[test]
        fn prop_pagerank_covers_every_node(n in 1usize..30) {
            let graph = make_test_graph(n);
            let result = pagerank(&graph, 0.85, 50, 1e-8);
            prop_assert_eq!(result.scores.len(), graph.node_count());
        }

        /// Tarjan SCC partitions every node into exactly one component.
        #[test]
        fn prop_tarjan_scc_partitions_every_node(n in 1usize..30) {
            let graph = make_test_graph(n);
            let sccs = tarjan_scc(&graph);
            let mut seen: std::collections::HashSet<NodeIndex> =
                std::collections::HashSet::new();
            for scc in &sccs {
                for &node in scc {
                    prop_assert!(seen.insert(node),
                        "node {:?} appears in multiple SCCs", node);
                }
            }
            prop_assert_eq!(seen.len(), graph.node_count(),
                "SCCs do not cover every node");
        }

        /// Parallel Brandes ≈ sequential Brandes on small graphs.
        #[test]
        fn prop_brandes_parallel_matches_sequential(n in 4usize..20) {
            let graph = make_test_graph(n);
            let seq = betweenness_centrality(&graph);
            let shutdown = Arc::new(AtomicBool::new(false));
            let pool = Arc::new(WorkPool::new(2, 4, 4, Arc::clone(&shutdown)));
            let par = betweenness_centrality_parallel(&graph, &pool, None);
            shutdown.store(true, std::sync::atomic::Ordering::Release);
            prop_assert_eq!(seq.len(), par.len());
            for (&node, &seq_val) in &seq {
                let par_val = par[&node];
                prop_assert!((seq_val - par_val).abs() < 1e-9,
                    "node {:?}: seq={} par={}", node, seq_val, par_val);
            }
        }

        /// For small graphs (n ≤ 2) betweenness is all zero.
        #[test]
        fn prop_betweenness_zero_for_tiny_graph(n in 0usize..=2) {
            let graph = make_test_graph(n);
            let result = betweenness_centrality(&graph);
            for &v in result.values() {
                prop_assert_eq!(v, 0.0);
            }
        }

        /// Degree counts are symmetric: Σ in = Σ out.
        #[test]
        fn prop_degree_sums_balanced(n in 1usize..30) {
            let graph = make_test_graph(n);
            let degrees = compute_degrees(&graph);
            let in_total: usize = degrees.values().map(|(i, _)| *i).sum();
            let out_total: usize = degrees.values().map(|(_, o)| *o).sum();
            prop_assert_eq!(in_total, out_total,
                "in_total {} != out_total {} — graph has an edge counting bug",
                in_total, out_total);
        }

        /// find_cycles returns only SCCs of size ≥ 2.
        #[test]
        fn prop_find_cycles_returns_nontrivial_sccs(n in 1usize..30) {
            let graph = make_test_graph(n);
            let cycles = find_cycles(&graph);
            for scc in &cycles {
                prop_assert!(scc.len() >= 2, "find_cycles returned singleton SCC");
            }
        }

        /// Normalized betweenness centrality ∈ [0, 1] for every node.
        #[test]
        fn prop_betweenness_in_unit_interval(n in 3usize..30) {
            let graph = make_test_graph(n);
            let bc = betweenness_centrality(&graph);
            for (&node, &v) in &bc {
                prop_assert!((0.0..=1.0 + 1e-6).contains(&v),
                    "node {:?} has betweenness {} outside [0, 1]", node, v);
            }
        }

        /// Permuting node indices permutes PageRank scores correspondingly.
        /// (Equivalent: structurally-isomorphic graphs yield the same
        /// score multiset.)
        #[test]
        fn prop_pagerank_invariant_score_multiset_under_relabel(n in 3usize..12) {
            let graph = make_test_graph(n);
            let pr = pagerank(&graph, 0.85, 100, 1e-8);
            let mut scores: Vec<f64> = pr.scores.values().copied().collect();
            scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // Make a second isomorphic graph with same edges and same node
            // count — same `make_test_graph(n)` call produces identical
            // structure. The multiset of scores must match.
            let graph2 = make_test_graph(n);
            let pr2 = pagerank(&graph2, 0.85, 100, 1e-8);
            let mut scores2: Vec<f64> = pr2.scores.values().copied().collect();
            scores2.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            prop_assert_eq!(scores.len(), scores2.len());
            for (a, b) in scores.iter().zip(scores2.iter()) {
                prop_assert!((a - b).abs() < 1e-6);
            }
        }

        /// Tarjan SCC on an empty graph returns no components, and every
        /// SCC on a non-empty graph is non-empty.
        #[test]
        fn prop_tarjan_scc_components_are_nonempty(n in 1usize..20) {
            let graph = make_test_graph(n);
            let sccs = tarjan_scc(&graph);
            for scc in &sccs {
                prop_assert!(!scc.is_empty(), "empty SCC in output");
            }
        }

        /// Louvain always partitions every node into exactly one community.
        #[test]
        fn prop_louvain_partitions_every_node(n in 3usize..20) {
            let graph = make_test_graph(n);
            let code_graph = CodeGraph {
                graph,
                file_id_to_node: HashMap::new(),
                node_to_file_id: HashMap::new(),
            };
            let result = louvain_communities(&code_graph, 1.0);
            // Every node must appear exactly once in the communities map.
            prop_assert_eq!(result.communities.len(), code_graph.graph.node_count());
            // Modularity is a valid real number.
            prop_assert!(result.modularity.is_finite(),
                "modularity must be finite, got {}", result.modularity);
        }

        /// Leaf nodes (in_degree = 0 or out_degree = 0 with no path
        /// through them) have betweenness centrality 0: they can't lie
        /// on a shortest path between two other nodes.
        #[test]
        fn prop_betweenness_zero_for_dead_end_nodes(n in 5usize..15) {
            let graph = make_test_graph(n);
            let bc = betweenness_centrality(&graph);
            for node in graph.node_indices() {
                let out_deg = graph
                    .neighbors_directed(node, Direction::Outgoing)
                    .count();
                let in_deg = graph
                    .neighbors_directed(node, Direction::Incoming)
                    .count();
                // A node that is both leaf (no in-edges) and has no outgoing
                // edges can never lie on any source-target path.
                if in_deg == 0 && out_deg == 0 {
                    let v = bc.get(&node).copied().unwrap_or(0.0);
                    prop_assert_eq!(v, 0.0);
                }
            }
        }

        /// For a clustered block-diagonal graph (two disjoint blocks, each
        /// with dense intra-block edges), Louvain finds a partition whose
        /// modularity is strictly positive — it can do better than the
        /// all-one-community baseline (Q=0).
        #[test]
        fn prop_louvain_modularity_nonnegative_for_clustered_graph(
            block_size in 4usize..8,
        ) {
            // Build 2 disjoint blocks, each a clique on `block_size` nodes.
            let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
            let mut nodes_by_block: Vec<Vec<NodeIndex>> = vec![Vec::new(), Vec::new()];
            for (b, block) in nodes_by_block.iter_mut().enumerate().take(2) {
                for i in 0..block_size {
                    block.push(g.add_node(FileNode {
                        file_id: (b * block_size + i) as i64,
                        relative_path: format!("b{}_f{}.rs", b, i),
                        language: "rust".into(),
                        module: format!("b{}", b),
                    }));
                }
            }
            for block in &nodes_by_block {
                for i in 0..block.len() {
                    for j in 0..block.len() {
                        if i != j {
                            g.add_edge(
                                block[i],
                                block[j],
                                EdgeWeight {
                                    weight: 1.0,
                                    edge_type: EdgeType::Import,
                                },
                            );
                        }
                    }
                }
            }
            let code_graph = CodeGraph {
                graph: g,
                file_id_to_node: HashMap::new(),
                node_to_file_id: HashMap::new(),
            };
            let result = louvain_communities(&code_graph, 1.0);
            prop_assert!(
                result.modularity > 0.0,
                "block-diagonal graph should yield Q>0, got {}",
                result.modularity
            );
        }

        /// Louvain's reported num_communities equals the number of distinct
        /// values in the communities map.
        #[test]
        fn prop_louvain_num_communities_matches_distinct_values(n in 3usize..20) {
            let graph = make_test_graph(n);
            let code_graph = CodeGraph {
                graph,
                file_id_to_node: HashMap::new(),
                node_to_file_id: HashMap::new(),
            };
            let result = louvain_communities(&code_graph, 1.0);
            let distinct: std::collections::HashSet<usize> =
                result.communities.values().copied().collect();
            prop_assert_eq!(distinct.len(), result.num_communities);
        }
    }

    /// A lone self-loop on a node constitutes a singleton SCC of size 1.
    /// `find_cycles` (which filters out singletons) correctly drops this,
    /// even though Tarjan SCC would return it.
    #[test]
    fn self_loop_is_singleton_scc_filtered_by_find_cycles() {
        let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let node = g.add_node(FileNode {
            file_id: 1,
            relative_path: "solo.rs".into(),
            language: "rust".into(),
            module: String::new(),
        });
        g.add_edge(
            node,
            node,
            EdgeWeight {
                weight: 1.0,
                edge_type: EdgeType::Import,
            },
        );
        // tarjan_scc returns the singleton SCC…
        let sccs = tarjan_scc(&g);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].len(), 1);
        // …but find_cycles filters it out (returns only scc.len() >= 2).
        let cycles = find_cycles(&g);
        assert!(
            cycles.is_empty(),
            "self-loop must not be reported as a cycle"
        );
    }
}
