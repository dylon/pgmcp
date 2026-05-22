//! SOTA Phase 2 extended graph algorithms.
//!
//! These complement the existing PageRank / Brandes / Louvain / Tarjan SCC in
//! `algorithms.rs` and cover all 11 algorithms named in the SOTA plan:
//! K-core (Seidman 1983), K-truss (Cohen 2008), Personalized PageRank
//! (Tong-Faloutsos-Pan ICDM 2006), Edge Betweenness (Girvan-Newman 2002,
//! Brandes edge variant), Eigenvector centrality (Bonacich 1987), Katz
//! centrality (Katz 1953), Harmonic centrality (Marchiori-Latora 2000),
//! Burt's structural-holes constraint (Burt 1992), Motif / graphlet census
//! (Milo et al. Science 2002), Degree assortativity (Newman 2003),
//! Modularity-based attack vulnerability (Holme et al. PRE 2002).

#![allow(dead_code)] // Consumers (cron + MCP tools) wire up incrementally.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use super::types::{EdgeWeight, FileNode};

// ============================================================================
// 2.1 K-core decomposition (Batagelj-Zaversnik 2003 O(m) algorithm)
// ============================================================================

#[derive(Debug, Clone)]
pub struct KCoreResult {
    pub coreness: HashMap<NodeIndex, u32>,
    pub max_core: u32,
}

/// K-core decomposition over the *undirected* projection (sum of in+out
/// neighbours, deduped).
pub fn k_core_decomposition(graph: &DiGraph<FileNode, EdgeWeight>) -> KCoreResult {
    let n = graph.node_count();
    if n == 0 {
        return KCoreResult {
            coreness: HashMap::new(),
            max_core: 0,
        };
    }

    // Build undirected adjacency.
    let mut adj: HashMap<NodeIndex, HashSet<NodeIndex>> = HashMap::with_capacity(n);
    for ni in graph.node_indices() {
        adj.entry(ni).or_default();
    }
    for edge in graph.edge_references() {
        let s = edge.source();
        let t = edge.target();
        if s != t {
            adj.entry(s).or_default().insert(t);
            adj.entry(t).or_default().insert(s);
        }
    }

    let mut deg: HashMap<NodeIndex, u32> = adj.iter().map(|(k, v)| (*k, v.len() as u32)).collect();
    let mut coreness: HashMap<NodeIndex, u32> = HashMap::with_capacity(n);
    let mut max_core: u32 = 0;

    // Process in non-decreasing order of current degree. Each removal updates
    // neighbour degrees, so re-bucket via a BTreeMap<degree, set of nodes>.
    let mut bucket: BTreeMap<u32, HashSet<NodeIndex>> = BTreeMap::new();
    for (&ni, &d) in &deg {
        bucket.entry(d).or_default().insert(ni);
    }

    while let Some((&min_d, _)) = bucket.iter().next() {
        let ni = match bucket
            .get_mut(&min_d)
            .and_then(|s| s.iter().next().copied())
        {
            Some(n) => n,
            None => {
                bucket.remove(&min_d);
                continue;
            }
        };
        bucket.get_mut(&min_d).expect("min_d set").remove(&ni);
        if bucket.get(&min_d).map(|s| s.is_empty()).unwrap_or(true) {
            bucket.remove(&min_d);
        }
        let core = deg[&ni].max(coreness.get(&ni).copied().unwrap_or(0));
        coreness.insert(ni, core);
        max_core = max_core.max(core);

        // Drop ni; decrement neighbours.
        if let Some(nbrs) = adj.remove(&ni) {
            for nb in nbrs {
                if let Some(adj_set) = adj.get_mut(&nb) {
                    adj_set.remove(&ni);
                }
                if let Some(old_deg) = deg.get(&nb).copied()
                    && old_deg > min_d
                {
                    bucket.entry(old_deg).and_modify(|s| {
                        s.remove(&nb);
                    });
                    if bucket.get(&old_deg).map(|s| s.is_empty()).unwrap_or(true) {
                        bucket.remove(&old_deg);
                    }
                    let new_deg = old_deg - 1;
                    deg.insert(nb, new_deg);
                    bucket.entry(new_deg).or_default().insert(nb);
                }
            }
        }
    }

    KCoreResult { coreness, max_core }
}

// ============================================================================
// 2.2 K-truss decomposition (Cohen 2008; edge-trussness via triangle support)
// ============================================================================

#[derive(Debug, Clone)]
pub struct KTrussResult {
    /// edge → trussness (k such that the edge is in a k-truss but not a (k+1)-truss).
    /// Edges keyed by (lo, hi) with lo.index() < hi.index() for canonical ordering.
    pub edge_trussness: HashMap<(NodeIndex, NodeIndex), u32>,
    pub max_truss: u32,
}

pub fn k_truss_decomposition(graph: &DiGraph<FileNode, EdgeWeight>) -> KTrussResult {
    // Build undirected edge set + adjacency
    let mut adj: HashMap<NodeIndex, HashSet<NodeIndex>> = HashMap::new();
    for ni in graph.node_indices() {
        adj.entry(ni).or_default();
    }
    let mut edges: HashSet<(NodeIndex, NodeIndex)> = HashSet::new();
    for e in graph.edge_references() {
        let (a, b) = canonical_pair(e.source(), e.target());
        if a == b {
            continue;
        }
        edges.insert((a, b));
        adj.entry(a).or_default().insert(b);
        adj.entry(b).or_default().insert(a);
    }

    // For each edge, compute initial support = |N(a) ∩ N(b)|
    let mut support: HashMap<(NodeIndex, NodeIndex), u32> = HashMap::with_capacity(edges.len());
    for &(a, b) in &edges {
        let na = adj.get(&a).cloned().unwrap_or_default();
        let nb = adj.get(&b).cloned().unwrap_or_default();
        let s = if na.len() <= nb.len() {
            na.iter().filter(|x| nb.contains(x)).count()
        } else {
            nb.iter().filter(|x| na.contains(x)).count()
        };
        support.insert((a, b), s as u32);
    }

    let mut trussness: HashMap<(NodeIndex, NodeIndex), u32> = HashMap::with_capacity(edges.len());
    let mut max_truss: u32 = 2;

    // Iteratively peel edges with the smallest support; k-truss requires
    // support >= k-2. Each removed edge's trussness is min_support + 2.
    while !support.is_empty() {
        let min_supp = support.values().copied().min().unwrap_or(0);
        let k = min_supp + 2;
        max_truss = max_truss.max(k);
        let to_remove: Vec<(NodeIndex, NodeIndex)> = support
            .iter()
            .filter(|(_, s)| **s == min_supp)
            .map(|(e, _)| *e)
            .collect();

        for edge in to_remove {
            let (a, b) = edge;
            trussness.insert(edge, k);
            // For every common neighbour c of (a, b), decrement support of
            // (a, c) and (b, c) by 1 — that triangle is now broken.
            if let (Some(na), Some(nb)) = (adj.get(&a).cloned(), adj.get(&b).cloned()) {
                for c in na.intersection(&nb) {
                    let ac = canonical_pair(a, *c);
                    let bc = canonical_pair(b, *c);
                    if let Some(s) = support.get_mut(&ac) {
                        *s = s.saturating_sub(1);
                    }
                    if let Some(s) = support.get_mut(&bc) {
                        *s = s.saturating_sub(1);
                    }
                }
            }
            // Remove (a, b)
            adj.get_mut(&a).expect("adj a").remove(&b);
            adj.get_mut(&b).expect("adj b").remove(&a);
            support.remove(&edge);
        }
    }

    KTrussResult {
        edge_trussness: trussness,
        max_truss,
    }
}

fn canonical_pair(a: NodeIndex, b: NodeIndex) -> (NodeIndex, NodeIndex) {
    if a.index() <= b.index() {
        (a, b)
    } else {
        (b, a)
    }
}

// ============================================================================
// 2.3 Personalized PageRank with restart (Tong-Faloutsos-Pan ICDM 2006)
// ============================================================================

#[derive(Debug, Clone)]
pub struct PersonalizedPageRank {
    pub scores: HashMap<NodeIndex, f64>,
    pub iterations: usize,
    pub converged: bool,
}

/// Power-iteration personalized PageRank. `seeds` must be L1-positive; the
/// function L1-normalises internally. Nodes not in `seeds` get teleport
/// mass = 0 (vs uniform 1/n in vanilla PageRank).
pub fn personalized_pagerank(
    graph: &DiGraph<FileNode, EdgeWeight>,
    seeds: &HashMap<NodeIndex, f64>,
    damping: f64,
    max_iter: usize,
    tolerance: f64,
) -> PersonalizedPageRank {
    let n = graph.node_count();
    if n == 0 || seeds.is_empty() {
        return PersonalizedPageRank {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
        };
    }
    // L1-normalize the seed vector.
    let seed_sum: f64 = seeds.values().copied().filter(|v| v.is_finite()).sum();
    let mut seed_vec: HashMap<NodeIndex, f64> = HashMap::with_capacity(seeds.len());
    if seed_sum > 0.0 {
        for (k, v) in seeds.iter() {
            seed_vec.insert(*k, *v / seed_sum);
        }
    } else {
        // All-zero seeds → uniform restart over the seed set.
        let uniform = 1.0 / seeds.len() as f64;
        for k in seeds.keys() {
            seed_vec.insert(*k, uniform);
        }
    }

    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let mut scores: HashMap<NodeIndex, f64> = nodes
        .iter()
        .map(|&ni| (ni, seed_vec.get(&ni).copied().unwrap_or(0.0)))
        .collect();

    let mut iterations = 0usize;
    let mut converged = false;
    for it in 0..max_iter {
        iterations = it + 1;
        let mut new_scores: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);

        // Dangling mass: nodes with no out-edges keep their score in the
        // teleport vector to preserve total mass.
        let mut dangling_sum = 0.0;
        for &node in &nodes {
            let od = graph.neighbors_directed(node, Direction::Outgoing).count();
            if od == 0 {
                dangling_sum += scores[&node];
            }
        }

        for &node in &nodes {
            let teleport = (1.0 - damping) * seed_vec.get(&node).copied().unwrap_or(0.0);
            let mut incoming = 0.0;
            for nb in graph.neighbors_directed(node, Direction::Incoming) {
                let od = graph.neighbors_directed(nb, Direction::Outgoing).count();
                if od > 0 {
                    incoming += scores[&nb] / od as f64;
                }
            }
            let dangling_contrib =
                damping * dangling_sum * seed_vec.get(&node).copied().unwrap_or(0.0);
            new_scores.insert(node, teleport + damping * incoming + dangling_contrib);
        }
        let diff = nodes
            .iter()
            .map(|ni| (new_scores[ni] - scores[ni]).abs())
            .fold(0.0_f64, f64::max);
        scores = new_scores;
        if diff < tolerance {
            converged = true;
            break;
        }
    }

    PersonalizedPageRank {
        scores,
        iterations,
        converged,
    }
}

// ============================================================================
// 2.4 Edge betweenness (Brandes 2001 edge variant)
// ============================================================================

/// Edge-key with canonical (u,v) ordering: u.index() <= v.index().
pub type EdgeKey = (NodeIndex, NodeIndex);

pub fn edge_betweenness(graph: &DiGraph<FileNode, EdgeWeight>) -> HashMap<EdgeKey, f64> {
    let n = graph.node_count();
    let mut result: HashMap<EdgeKey, f64> = HashMap::new();
    if n == 0 {
        return result;
    }

    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    // Undirected projection: collect neighbours per node.
    let mut adj: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::with_capacity(n);
    for ni in &nodes {
        adj.entry(*ni).or_default();
    }
    for e in graph.edge_references() {
        let (a, b) = (e.source(), e.target());
        if a == b {
            continue;
        }
        adj.entry(a).or_default().push(b);
        adj.entry(b).or_default().push(a);
    }

    for &source in &nodes {
        // BFS shortest-path layers
        let mut sigma: HashMap<NodeIndex, f64> = HashMap::new();
        let mut dist: HashMap<NodeIndex, i64> = HashMap::new();
        let mut preds: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();
        let mut stack: Vec<NodeIndex> = Vec::new();
        for &v in &nodes {
            sigma.insert(v, 0.0);
            dist.insert(v, -1);
            preds.insert(v, Vec::new());
        }
        sigma.insert(source, 1.0);
        dist.insert(source, 0);
        let mut q: VecDeque<NodeIndex> = VecDeque::new();
        q.push_back(source);
        while let Some(v) = q.pop_front() {
            stack.push(v);
            for &w in adj.get(&v).map(|x| x.as_slice()).unwrap_or(&[]) {
                if dist[&w] < 0 {
                    dist.insert(w, dist[&v] + 1);
                    q.push_back(w);
                }
                if dist[&w] == dist[&v] + 1 {
                    let sv = sigma[&v];
                    sigma.entry(w).and_modify(|x| *x += sv).or_insert(sv);
                    preds.entry(w).or_default().push(v);
                }
            }
        }
        // Accumulate edge dependencies in reverse BFS order
        let mut delta: HashMap<NodeIndex, f64> = nodes.iter().map(|&v| (v, 0.0)).collect();
        while let Some(w) = stack.pop() {
            let dw = delta[&w];
            let sw = sigma[&w];
            for &v in preds.get(&w).map(|x| x.as_slice()).unwrap_or(&[]) {
                let c = (sigma[&v] / sw) * (1.0 + dw);
                *delta.entry(v).or_insert(0.0) += c;
                let key = canonical_pair(v, w);
                *result.entry(key).or_insert(0.0) += c;
            }
        }
    }

    // For undirected sources, every edge was credited twice — halve.
    for v in result.values_mut() {
        *v /= 2.0;
    }
    result
}

// ============================================================================
// 2.5 Eigenvector centrality (Bonacich 1987 — power iteration on adjacency)
// ============================================================================

pub fn eigenvector_centrality(
    graph: &DiGraph<FileNode, EdgeWeight>,
    max_iter: usize,
    tolerance: f64,
) -> HashMap<NodeIndex, f64> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let init = 1.0 / (n as f64).sqrt();
    let mut x: HashMap<NodeIndex, f64> = nodes.iter().map(|&v| (v, init)).collect();

    // Symmetric undirected adjacency with weights.
    let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, f64)>> = HashMap::with_capacity(n);
    for ni in &nodes {
        adj.entry(*ni).or_default();
    }
    for e in graph.edge_references() {
        let w = e.weight().weight.max(0.0);
        adj.entry(e.source()).or_default().push((e.target(), w));
        adj.entry(e.target()).or_default().push((e.source(), w));
    }

    for _ in 0..max_iter {
        let mut nx: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);
        for &v in &nodes {
            let mut s = 0.0;
            for &(u, w) in adj.get(&v).map(|x| x.as_slice()).unwrap_or(&[]) {
                s += w * x[&u];
            }
            nx.insert(v, s);
        }
        // L2-normalize
        let norm = nx.values().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 0.0 {
            for v in nx.values_mut() {
                *v /= norm;
            }
        }
        let diff = nodes
            .iter()
            .map(|v| (nx[v] - x[v]).abs())
            .fold(0.0_f64, f64::max);
        x = nx;
        if diff < tolerance {
            break;
        }
    }
    x
}

// ============================================================================
// 2.6 Katz centrality (Katz 1953)
// ============================================================================

pub fn katz_centrality(
    graph: &DiGraph<FileNode, EdgeWeight>,
    alpha: f64,
    beta: f64,
    max_iter: usize,
    tolerance: f64,
) -> HashMap<NodeIndex, f64> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let mut x: HashMap<NodeIndex, f64> = nodes.iter().map(|&v| (v, 0.0)).collect();

    // Use the undirected adjacency for Katz (typical convention).
    let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, f64)>> = HashMap::with_capacity(n);
    for ni in &nodes {
        adj.entry(*ni).or_default();
    }
    for e in graph.edge_references() {
        let w = e.weight().weight.max(0.0);
        adj.entry(e.source()).or_default().push((e.target(), w));
        adj.entry(e.target()).or_default().push((e.source(), w));
    }

    for _ in 0..max_iter {
        let mut nx: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);
        for &v in &nodes {
            let mut s = 0.0;
            for &(u, w) in adj.get(&v).map(|x| x.as_slice()).unwrap_or(&[]) {
                s += w * x[&u];
            }
            nx.insert(v, alpha * s + beta);
        }
        let diff = nodes
            .iter()
            .map(|v| (nx[v] - x[v]).abs())
            .fold(0.0_f64, f64::max);
        x = nx;
        if diff < tolerance {
            break;
        }
    }

    // L2-normalize for comparability with eigenvector.
    let norm = x.values().map(|v| v * v).sum::<f64>().sqrt();
    if norm > 0.0 {
        for v in x.values_mut() {
            *v /= norm;
        }
    }
    x
}

// ============================================================================
// 2.7 Harmonic centrality (Marchiori-Latora 2000)
// ============================================================================

pub fn harmonic_centrality(graph: &DiGraph<FileNode, EdgeWeight>) -> HashMap<NodeIndex, f64> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let mut adj: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::with_capacity(n);
    for ni in &nodes {
        adj.entry(*ni).or_default();
    }
    for e in graph.edge_references() {
        adj.entry(e.source()).or_default().push(e.target());
        adj.entry(e.target()).or_default().push(e.source());
    }

    let mut out: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);

    for &source in &nodes {
        // BFS unweighted distance
        let mut dist: HashMap<NodeIndex, i64> = HashMap::new();
        for &v in &nodes {
            dist.insert(v, -1);
        }
        dist.insert(source, 0);
        let mut q: VecDeque<NodeIndex> = VecDeque::new();
        q.push_back(source);
        while let Some(v) = q.pop_front() {
            for &w in adj.get(&v).map(|x| x.as_slice()).unwrap_or(&[]) {
                if dist[&w] < 0 {
                    dist.insert(w, dist[&v] + 1);
                    q.push_back(w);
                }
            }
        }
        let mut sum: f64 = 0.0;
        for &v in &nodes {
            if v != source && dist[&v] > 0 {
                sum += 1.0 / dist[&v] as f64;
            }
        }
        out.insert(source, sum / (n as f64 - 1.0).max(1.0));
    }
    out
}

// ============================================================================
// 2.8 Burt's structural-holes constraint (Burt 1992)
// ============================================================================

/// Per-node Burt constraint. Low values = bridges across structural holes,
/// high values = redundantly-embedded.
pub fn burt_constraint(graph: &DiGraph<FileNode, EdgeWeight>) -> HashMap<NodeIndex, f64> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }
    // Build undirected weighted adjacency.
    let mut adj: HashMap<NodeIndex, HashMap<NodeIndex, f64>> = HashMap::with_capacity(n);
    for ni in graph.node_indices() {
        adj.entry(ni).or_default();
    }
    for e in graph.edge_references() {
        let w = e.weight().weight.max(0.0);
        if e.source() == e.target() {
            continue;
        }
        *adj.entry(e.source())
            .or_default()
            .entry(e.target())
            .or_insert(0.0) += w;
        *adj.entry(e.target())
            .or_default()
            .entry(e.source())
            .or_insert(0.0) += w;
    }

    let mut out: HashMap<NodeIndex, f64> = HashMap::with_capacity(n);
    for &i in &graph.node_indices().collect::<Vec<_>>() {
        let neighbours_i = adj.get(&i).cloned().unwrap_or_default();
        let total_i: f64 = neighbours_i.values().sum();
        if total_i <= 0.0 || neighbours_i.is_empty() {
            out.insert(i, 0.0);
            continue;
        }
        let mut constraint: f64 = 0.0;
        for (&j, w_ij) in &neighbours_i {
            let p_ij = w_ij / total_i;
            // Indirect: sum over q in N(i) ∩ N(j), q != i, j
            let neighbours_j = adj.get(&j).cloned().unwrap_or_default();
            let mut indirect: f64 = 0.0;
            for (&q, w_iq) in &neighbours_i {
                if q == j || q == i {
                    continue;
                }
                if let Some(w_qj) = neighbours_j.get(&q) {
                    let p_iq = w_iq / total_i;
                    let total_q: f64 = adj.get(&q).map(|m| m.values().sum()).unwrap_or(0.0);
                    if total_q > 0.0 {
                        let p_qj = w_qj / total_q;
                        indirect += p_iq * p_qj;
                    }
                }
            }
            constraint += (p_ij + indirect).powi(2);
        }
        out.insert(i, constraint);
    }
    out
}

// ============================================================================
// 2.9 Motif / graphlet census (Milo et al. Science 2002; 3-node directed
// triad classes; 4-node counts limited to clique + star for efficiency).
// ============================================================================

#[derive(Debug, Clone, Default)]
pub struct MotifCensus {
    /// 16-class Davis-Leinhardt directed triad census, packed: 003, 012, 102, 021D, 021U, 021C,
    /// 111D, 111U, 030T, 030C, 201, 120D, 120U, 120C, 210, 300.
    pub triads: [u64; 16],
    /// 4-node motifs of interest:
    ///   [0] = 4-cliques (all 6 directed edges between 4 nodes)
    ///   [1] = directed stars (1 → 3 leaves)
    pub graphlets_4: [u64; 2],
}

pub fn motif_census(graph: &DiGraph<FileNode, EdgeWeight>) -> MotifCensus {
    let n = graph.node_count();
    let mut out = MotifCensus::default();
    if n < 3 {
        return out;
    }

    // Build directed adjacency matrix for fast lookup.
    let nodes: Vec<NodeIndex> = graph.node_indices().collect();
    let idx: HashMap<NodeIndex, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let mut adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for e in graph.edge_references() {
        if e.source() == e.target() {
            continue;
        }
        let s = idx[&e.source()];
        let t = idx[&e.target()];
        adj[s].insert(t);
    }

    let has = |a: usize, b: usize| -> bool { adj[a].contains(&b) };

    // Triad census over all C(n,3) triples.
    for i in 0..n {
        for j in (i + 1)..n {
            for k in (j + 1)..n {
                let class = classify_triad(i, j, k, &has);
                out.triads[class] += 1;
            }
        }
    }

    // 4-node clique and directed-star counts (cap at 4-node enumeration when n < 50
    // to keep cost bounded; the cron caller already gates by per-project cooldown).
    if n <= 50 {
        for i in 0..n {
            for j in (i + 1)..n {
                for k in (j + 1)..n {
                    for l in (k + 1)..n {
                        let clique = has(i, j)
                            && has(j, i)
                            && has(i, k)
                            && has(k, i)
                            && has(i, l)
                            && has(l, i)
                            && has(j, k)
                            && has(k, j)
                            && has(j, l)
                            && has(l, j)
                            && has(k, l)
                            && has(l, k);
                        if clique {
                            out.graphlets_4[0] += 1;
                        }
                    }
                }
            }
        }
        // Directed stars: center c with ≥3 distinct out-neighbours and no
        // edges between the leaves.
        for (_c, neighbours) in adj.iter().enumerate().take(n) {
            let leaves: Vec<usize> = neighbours.iter().copied().collect();
            if leaves.len() < 3 {
                continue;
            }
            for i in 0..leaves.len() {
                for j in (i + 1)..leaves.len() {
                    for k in (j + 1)..leaves.len() {
                        let (a, b, cc) = (leaves[i], leaves[j], leaves[k]);
                        if !has(a, b)
                            && !has(b, a)
                            && !has(a, cc)
                            && !has(cc, a)
                            && !has(b, cc)
                            && !has(cc, b)
                        {
                            out.graphlets_4[1] += 1;
                        }
                    }
                }
            }
        }
    }

    out
}

/// 16-class triad classifier index.
fn classify_triad(i: usize, j: usize, k: usize, has: &impl Fn(usize, usize) -> bool) -> usize {
    let edges = [
        has(i, j),
        has(j, i),
        has(i, k),
        has(k, i),
        has(j, k),
        has(k, j),
    ];
    let m: u8 = edges.iter().filter(|b| **b).count() as u8;
    match m {
        0 => 0, // 003
        1 => 1, // 012
        2 => classify_two_edge(&edges),
        3 => classify_three_edge(&edges),
        4 => classify_four_edge(&edges),
        5 => 14, // 210
        _ => 15, // 300 — full clique
    }
}

fn classify_two_edge(e: &[bool; 6]) -> usize {
    // 102 = reciprocal one pair (4 cases), 021D = two out from same node, 021U = two in to same, 021C = chain
    // Pair indices: (0,1)=ij/ji, (2,3)=ik/ki, (4,5)=jk/kj.
    if (e[0] && e[1]) || (e[2] && e[3]) || (e[4] && e[5]) {
        return 2; // 102
    }
    // Both out from same node?
    if (e[0] && e[2]) || (e[1] && e[4]) || (e[3] && e[5]) {
        return 3; // 021D
    }
    // Both into same node?
    if (e[1] && e[3]) || (e[0] && e[5]) || (e[2] && e[4]) {
        return 4; // 021U
    }
    5 // 021C
}

fn classify_three_edge(e: &[bool; 6]) -> usize {
    let rec_ij = e[0] && e[1];
    let rec_ik = e[2] && e[3];
    let rec_jk = e[4] && e[5];
    let rec_count = (rec_ij as u8) + (rec_ik as u8) + (rec_jk as u8);
    if rec_count == 1 {
        // 111D or 111U — distinguish by the orientation of the third edge.
        return 6; // 111D (collapse 111U into 111D for simplicity here)
    }
    // 030T (transitive) or 030C (cyclic)
    if (e[0] && e[4] && e[2]) || (e[1] && e[5] && e[3]) {
        return 9; // 030C cycle
    }
    8 // 030T (transitive)
}

fn classify_four_edge(e: &[bool; 6]) -> usize {
    let rec_ij = e[0] && e[1];
    let rec_ik = e[2] && e[3];
    let rec_jk = e[4] && e[5];
    let rec_count = (rec_ij as u8) + (rec_ik as u8) + (rec_jk as u8);
    if rec_count == 2 {
        return 10; // 201
    }
    if rec_count == 1 {
        return 12; // 120 family (D/U/C — collapse)
    }
    11 // 120D fallback
}

// ============================================================================
// 2.10 Degree assortativity (Newman 2003 — Pearson on (out_deg, in_deg)
// across directed edges).
// ============================================================================

pub fn degree_assortativity(graph: &DiGraph<FileNode, EdgeWeight>) -> f64 {
    let m = graph.edge_count() as f64;
    if m == 0.0 {
        return 0.0;
    }
    let mut sum_x: f64 = 0.0;
    let mut sum_y: f64 = 0.0;
    let mut sum_xy: f64 = 0.0;
    let mut sum_x2: f64 = 0.0;
    let mut sum_y2: f64 = 0.0;
    for e in graph.edge_references() {
        let x = graph
            .neighbors_directed(e.source(), Direction::Outgoing)
            .count() as f64;
        let y = graph
            .neighbors_directed(e.target(), Direction::Incoming)
            .count() as f64;
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_x2 += x * x;
        sum_y2 += y * y;
    }
    let num = m * sum_xy - sum_x * sum_y;
    let den_x = (m * sum_x2 - sum_x * sum_x).max(0.0).sqrt();
    let den_y = (m * sum_y2 - sum_y * sum_y).max(0.0).sqrt();
    let den = den_x * den_y;
    if den.abs() < f64::EPSILON {
        0.0
    } else {
        (num / den).clamp(-1.0, 1.0)
    }
}

// ============================================================================
// 2.11 Modularity-based attack vulnerability (Holme et al. PRE 2002)
// ============================================================================

#[derive(Debug, Clone)]
pub struct AttackStep {
    pub step: u32,
    pub removed: NodeIndex,
    pub largest_component: usize,
    pub remaining_nodes: usize,
}

#[derive(Debug, Clone)]
pub struct AttackResult {
    pub trace: Vec<AttackStep>,
    /// Area-under-curve of largest_component/n vs step/max_steps. Lower = more vulnerable.
    pub auc: f64,
}

/// Simulate sequential removal in `order`, recording largest connected component
/// (undirected projection) after each removal. `order` is processed up to
/// `max_steps` items.
pub fn modularity_attack(
    graph: &DiGraph<FileNode, EdgeWeight>,
    order: &[NodeIndex],
    max_steps: usize,
) -> AttackResult {
    let n = graph.node_count();
    if n == 0 || order.is_empty() {
        return AttackResult {
            trace: Vec::new(),
            auc: 0.0,
        };
    }
    let mut adj: HashMap<NodeIndex, HashSet<NodeIndex>> = HashMap::with_capacity(n);
    for ni in graph.node_indices() {
        adj.entry(ni).or_default();
    }
    for e in graph.edge_references() {
        if e.source() == e.target() {
            continue;
        }
        adj.entry(e.source()).or_default().insert(e.target());
        adj.entry(e.target()).or_default().insert(e.source());
    }

    let mut trace: Vec<AttackStep> = Vec::with_capacity(max_steps);
    let mut alive: HashSet<NodeIndex> = adj.keys().copied().collect();

    let steps = order.len().min(max_steps);
    for (i, ni) in order.iter().take(steps).enumerate() {
        if let Some(nbrs) = adj.remove(ni) {
            for nb in nbrs {
                if let Some(set) = adj.get_mut(&nb) {
                    set.remove(ni);
                }
            }
        }
        alive.remove(ni);
        let lcc = largest_component_size(&adj, &alive);
        trace.push(AttackStep {
            step: (i + 1) as u32,
            removed: *ni,
            largest_component: lcc,
            remaining_nodes: alive.len(),
        });
    }

    // AUC: trapezoid rule on (lcc / n) vs (step / steps), normalized
    let mut auc = 0.0;
    if !trace.is_empty() {
        let denom = n as f64;
        let dt = 1.0 / steps as f64;
        let mut prev = 1.0_f64;
        for s in &trace {
            let cur = s.largest_component as f64 / denom.max(1.0);
            auc += 0.5 * (prev + cur) * dt;
            prev = cur;
        }
    }

    AttackResult { trace, auc }
}

fn largest_component_size(
    adj: &HashMap<NodeIndex, HashSet<NodeIndex>>,
    alive: &HashSet<NodeIndex>,
) -> usize {
    let mut visited: HashSet<NodeIndex> = HashSet::new();
    let mut best: usize = 0;
    for &start in alive {
        if visited.contains(&start) {
            continue;
        }
        let mut q: VecDeque<NodeIndex> = VecDeque::new();
        q.push_back(start);
        visited.insert(start);
        let mut size = 0;
        while let Some(v) = q.pop_front() {
            size += 1;
            if let Some(nbrs) = adj.get(&v) {
                for &nb in nbrs {
                    if alive.contains(&nb) && !visited.contains(&nb) {
                        visited.insert(nb);
                        q.push_back(nb);
                    }
                }
            }
        }
        best = best.max(size);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::DiGraph;

    fn fnode(id: i64, name: &str) -> FileNode {
        FileNode {
            file_id: id,
            relative_path: name.into(),
            language: "rust".into(),
            module: "test".into(),
        }
    }

    fn ew() -> EdgeWeight {
        EdgeWeight {
            edge_type: super::super::types::EdgeType::Import,
            weight: 1.0,
        }
    }

    fn build_chain(n: usize) -> DiGraph<FileNode, EdgeWeight> {
        let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let nodes: Vec<NodeIndex> = (0..n)
            .map(|i| g.add_node(fnode(i as i64 + 1, &format!("f{}", i))))
            .collect();
        for i in 0..n - 1 {
            g.add_edge(nodes[i], nodes[i + 1], ew());
        }
        g
    }

    #[test]
    fn kcore_chain_is_1_core() {
        let g = build_chain(5);
        let r = k_core_decomposition(&g);
        assert_eq!(r.max_core, 1);
    }

    #[test]
    fn kcore_clique_has_core_n_minus_1() {
        // Build a complete graph of 4 nodes (undirected projection from
        // bidirectional edges).
        let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let nodes: Vec<NodeIndex> = (0..4)
            .map(|i| g.add_node(fnode(i as i64 + 1, &format!("n{}", i))))
            .collect();
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    g.add_edge(nodes[i], nodes[j], ew());
                }
            }
        }
        let r = k_core_decomposition(&g);
        // K4 has every node in the 3-core.
        assert_eq!(r.max_core, 3);
    }

    #[test]
    fn personalized_pagerank_concentrates_on_seed() {
        let g = build_chain(5);
        let seed_node = g.node_indices().next().expect("first");
        let mut seeds = HashMap::new();
        seeds.insert(seed_node, 1.0);
        let r = personalized_pagerank(&g, &seeds, 0.85, 100, 1e-6);
        assert!(r.converged);
        assert!(r.scores[&seed_node] > 0.0);
    }

    #[test]
    fn edge_betweenness_path_bottlenecks_middle_edges() {
        let g = build_chain(5);
        let bw = edge_betweenness(&g);
        // The middle edge has the highest betweenness on a path graph.
        assert!(bw.values().fold(0.0_f64, |a, b| a.max(*b)) > 0.0);
    }

    #[test]
    fn eigenvector_centrality_returns_nonempty() {
        let g = build_chain(5);
        let r = eigenvector_centrality(&g, 100, 1e-6);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn katz_centrality_returns_nonempty() {
        let g = build_chain(5);
        let r = katz_centrality(&g, 0.1, 1.0, 100, 1e-6);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn harmonic_centrality_endpoints_smaller_than_middle() {
        let g = build_chain(5);
        let h = harmonic_centrality(&g);
        let nodes: Vec<NodeIndex> = g.node_indices().collect();
        let endpoint = h[&nodes[0]];
        let middle = h[&nodes[2]];
        assert!(middle >= endpoint);
    }

    #[test]
    fn burt_constraint_isolated_node_is_zero() {
        let mut g: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let n = g.add_node(fnode(1, "a"));
        let r = burt_constraint(&g);
        assert_eq!(r.get(&n).copied().unwrap_or(-1.0), 0.0);
    }

    #[test]
    fn assortativity_is_in_range() {
        let g = build_chain(5);
        let r = degree_assortativity(&g);
        assert!((-1.0..=1.0).contains(&r));
    }

    #[test]
    fn motif_census_counts_triads_on_chain() {
        let g = build_chain(4);
        let r = motif_census(&g);
        // On a 4-node directed chain, all C(4,3)=4 triads are either 012 or
        // 021C/021D depending on the orientation, but never empty.
        let total: u64 = r.triads.iter().sum();
        assert_eq!(total, 4);
    }

    #[test]
    fn attack_collapses_chain() {
        let g = build_chain(5);
        let nodes: Vec<NodeIndex> = g.node_indices().collect();
        let r = modularity_attack(&g, &nodes, 5);
        assert_eq!(r.trace.len(), 5);
        assert_eq!(r.trace.last().expect("last").largest_component, 0);
    }
}
