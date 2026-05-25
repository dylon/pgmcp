//! Spectral graph analysis: algebraic connectivity (Fiedler value) + the
//! Fiedler vector for a balanced normalized-cut bipartition. (graph-roadmap
//! Phase 4.6)
//!
//! - **Algebraic connectivity** λ₂ (Fiedler 1973): the second-smallest
//!   eigenvalue of the graph Laplacian `L = D − A`. λ₂ = 0 iff the graph is
//!   disconnected; small λ₂ = a near-bottleneck (a weak global seam); large λ₂ =
//!   robustly connected.
//! - **Fiedler vector**: λ₂'s eigenvector; its sign split is the classic
//!   spectral bisection (Shi-Malik normalized cut, PAMI 2000) — a natural,
//!   balanced module boundary.
//!
//! Computed by deflated power iteration on `(cI − L)`: the constant vector
//! (λ=0) is projected out each step, so the iteration converges to the
//! eigenvector of the smallest *positive* Laplacian eigenvalue. Pure + generic
//! over `DiGraph<N, E>` (undirected, unit-weight projection); O(iters·(n+e)).

use petgraph::graph::DiGraph;
use petgraph::visit::EdgeRef;

/// Result of the spectral analysis.
#[derive(Debug, Clone)]
pub struct Spectral {
    /// Algebraic connectivity λ₂ (≈0 ⇒ disconnected / bottleneck).
    pub algebraic_connectivity: f64,
    /// Fiedler vector, indexed by `NodeIndex::index()`.
    pub fiedler: Vec<f64>,
    /// Whether the power iteration met the tolerance.
    pub converged: bool,
}

/// Compute algebraic connectivity + Fiedler vector over the undirected,
/// unit-weight projection of `graph`. `None` for graphs with < 2 nodes.
pub fn algebraic_connectivity<N, E>(graph: &DiGraph<N, E>) -> Option<Spectral> {
    let n = graph.node_count();
    if n < 2 {
        return None;
    }

    // Undirected unit adjacency (dedup self-loops + parallel edges into a set is
    // unnecessary — repeated edges just raise the weight, fine for the Laplacian).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut deg: Vec<f64> = vec![0.0; n];
    for e in graph.edge_references() {
        let (a, b) = (e.source().index(), e.target().index());
        if a == b {
            continue;
        }
        adj[a].push(b);
        adj[b].push(a);
        deg[a] += 1.0;
        deg[b] += 1.0;
    }

    // Spectral-radius bound for L: λ_max ≤ 2·max_degree. Use c above that so
    // (cI − L) is positive definite and its top eigenpair (after deflating the
    // constant) is the Fiedler pair.
    let max_deg = deg.iter().cloned().fold(0.0_f64, f64::max);
    let c = 2.0 * max_deg + 1.0;

    // Deterministic non-constant init, orthogonalized to the all-ones vector.
    let mut v: Vec<f64> = (0..n)
        .map(|i| (i as f64) - (n as f64 - 1.0) / 2.0)
        .collect();
    orthonormalize(&mut v);

    let mut converged = false;
    let mut lambda2 = 0.0;
    for _ in 0..300 {
        // w = (cI − L) v = c·v − (D·v − A·v) = c·v − deg∘v + A·v
        let mut w = vec![0.0; n];
        for i in 0..n {
            let mut neigh = 0.0;
            for &j in &adj[i] {
                neigh += v[j];
            }
            w[i] = c * v[i] - deg[i] * v[i] + neigh;
        }
        orthonormalize(&mut w); // project out the constant vector + normalize
        // Rayleigh quotient of L on w gives λ (since w ⟂ 1).
        let lam = rayleigh_l(&w, &adj, &deg);
        let delta: f64 = w.iter().zip(&v).map(|(a, b)| (a - b).abs()).sum();
        v = w;
        if (lam - lambda2).abs() < 1e-9 && delta < 1e-9 {
            lambda2 = lam;
            converged = true;
            break;
        }
        lambda2 = lam;
    }

    Some(Spectral {
        algebraic_connectivity: lambda2.max(0.0),
        fiedler: v,
        converged,
    })
}

/// vᵀ L v / vᵀ v for L = D − A (v assumed already unit-norm ⇒ denom 1).
fn rayleigh_l(v: &[f64], adj: &[Vec<usize>], deg: &[f64]) -> f64 {
    // vᵀ L v = Σ_i deg_i v_i² − Σ_{(i,j)∈E,both dirs} v_i v_j
    let mut quad = 0.0;
    for i in 0..v.len() {
        quad += deg[i] * v[i] * v[i];
        let mut cross = 0.0;
        for &j in &adj[i] {
            cross += v[i] * v[j];
        }
        quad -= cross;
    }
    quad // denom == 1 after orthonormalize
}

/// Subtract the mean (project onto the all-ones complement) and L2-normalize.
fn orthonormalize(v: &mut [f64]) {
    let n = v.len() as f64;
    if n == 0.0 {
        return;
    }
    let mean = v.iter().sum::<f64>() / n;
    for x in v.iter_mut() {
        *x -= mean;
    }
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::NodeIndex;

    fn graph_from(n: usize, edges: &[(usize, usize)]) -> DiGraph<(), ()> {
        let mut g = DiGraph::<(), ()>::new();
        let idx: Vec<NodeIndex> = (0..n).map(|_| g.add_node(())).collect();
        for &(s, t) in edges {
            g.add_edge(idx[s], idx[t], ());
        }
        g
    }

    #[test]
    fn complete_triangle_connectivity_is_n() {
        // K_3 algebraic connectivity = 3.
        let g = graph_from(3, &[(0, 1), (1, 2), (2, 0)]);
        let s = algebraic_connectivity(&g).expect("≥2 nodes");
        assert!(
            (s.algebraic_connectivity - 3.0).abs() < 1e-3,
            "λ₂(K₃) ≈ 3, got {}",
            s.algebraic_connectivity
        );
    }

    #[test]
    fn disconnected_graph_has_zero_connectivity() {
        // Two separate edges: disconnected ⇒ λ₂ ≈ 0.
        let g = graph_from(4, &[(0, 1), (2, 3)]);
        let s = algebraic_connectivity(&g).expect("≥2 nodes");
        assert!(
            s.algebraic_connectivity < 1e-6,
            "disconnected ⇒ λ₂≈0, got {}",
            s.algebraic_connectivity
        );
    }

    #[test]
    fn path_graph_fiedler_splits_halves() {
        // P₄: 0-1-2-3. λ₂ = 2(1−cos(π/4)) ≈ 0.586; Fiedler vector is monotone,
        // so its sign splits {0,1} | {2,3}.
        let g = graph_from(4, &[(0, 1), (1, 2), (2, 3)]);
        let s = algebraic_connectivity(&g).expect("≥2 nodes");
        assert!(
            (s.algebraic_connectivity - 0.586).abs() < 0.05,
            "λ₂(P₄) ≈ 0.586, got {}",
            s.algebraic_connectivity
        );
        // Ends have opposite signs (the cut is in the middle).
        assert!(
            s.fiedler[0] * s.fiedler[3] < 0.0,
            "Fiedler should separate the two ends: {:?}",
            s.fiedler
        );
    }
}
