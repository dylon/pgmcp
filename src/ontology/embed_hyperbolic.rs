//! Poincaré (hyperbolic) embedding of the `is_a` DAG for missing-edge link
//! prediction (Phase 8, optional ML). Nickel & Kiela (NeurIPS 2017): embed nodes
//! into the open Poincaré ball so the tree metric is preserved with far fewer
//! dimensions than Euclidean space; the **norm** of a point encodes its depth
//! (general concepts near the origin, specific ones near the boundary).
//!
//! Trained with Riemannian SGD on the contrastive softmax loss with negative
//! sampling. Pure, CPU-only, deterministic given a seed (a built-in xorshift PRNG
//! — no GPU, no `rand` dependency). Link prediction proposes `child is_a parent`
//! for unlinked pairs that are hyperbolically close AND strictly ordered by norm
//! (child deeper than parent) — strict ordering ⇒ the proposed edge set is acyclic.

/// Deterministic xorshift64* PRNG (avoids a `rand` dep + keeps training replayable).
struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform f64 in [-half, half].
    fn uniform(&mut self, half: f64) -> f64 {
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64; // [0,1)
        (u * 2.0 - 1.0) * half
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64)) as usize
    }
}

const EPS: f64 = 1e-5;
const BOUNDARY: f64 = 1.0 - 1e-3;

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn arccosh(x: f64) -> f64 {
    // d(x,x) gives arg == 1.0 exactly ⇒ distance 0; only x > 1 has positive distance.
    if x <= 1.0 {
        return 0.0;
    }
    (x + (x * x - 1.0).sqrt()).ln()
}

/// A trained Poincaré embedding over `n` nodes.
pub struct PoincareModel {
    dim: usize,
    emb: Vec<Vec<f64>>,
}

impl PoincareModel {
    /// Poincaré distance between nodes `i` and `j`.
    pub fn distance(&self, i: usize, j: usize) -> f64 {
        poincare_distance(&self.emb[i], &self.emb[j])
    }

    /// L2 norm of node `i` (its tree depth proxy: larger ⇒ more specific).
    pub fn norm(&self, i: usize) -> f64 {
        dot(&self.emb[i], &self.emb[i]).sqrt()
    }

    pub fn len(&self) -> usize {
        self.emb.len()
    }

    pub fn is_empty(&self) -> bool {
        self.emb.is_empty()
    }

    /// Propose `child is_a parent` edges for unlinked pairs that are within
    /// `max_dist` and strictly ordered by norm (child deeper). `existing` is the
    /// set of present directed `(child, parent)` edges (skipped). Returns
    /// `(child, parent, distance)` sorted nearest-first, capped at `top_k`. The
    /// strict norm ordering guarantees the proposals are acyclic.
    pub fn predict_missing(
        &self,
        existing: &std::collections::HashSet<(usize, usize)>,
        max_dist: f64,
        top_k: usize,
    ) -> Vec<(usize, usize, f64)> {
        let n = self.emb.len();
        let mut out: Vec<(usize, usize, f64)> = Vec::new();
        for c in 0..n {
            for p in 0..n {
                if c == p || existing.contains(&(c, p)) {
                    continue;
                }
                // child must be strictly deeper (more specific) than parent.
                if self.norm(c) <= self.norm(p) + EPS {
                    continue;
                }
                let d = self.distance(c, p);
                if d <= max_dist {
                    out.push((c, p, d));
                }
            }
        }
        out.sort_by(|a, b| a.2.total_cmp(&b.2));
        out.truncate(top_k);
        out
    }
}

fn poincare_distance(u: &[f64], v: &[f64]) -> f64 {
    let nu = dot(u, u).min(BOUNDARY * BOUNDARY);
    let nv = dot(v, v).min(BOUNDARY * BOUNDARY);
    let diff: f64 = u.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum();
    let denom = ((1.0 - nu) * (1.0 - nv)).max(EPS);
    arccosh(1.0 + 2.0 * diff / denom)
}

/// Project a point back inside the ball if it has escaped.
fn project(p: &mut [f64]) {
    let norm = dot(p, p).sqrt();
    if norm >= BOUNDARY {
        let scale = BOUNDARY / norm;
        for x in p.iter_mut() {
            *x *= scale;
        }
    }
}

/// Euclidean gradient of `d(theta, x)` w.r.t. `theta` (Nickel & Kiela eq. 4),
/// accumulated into `grad`.
fn dist_grad(theta: &[f64], x: &[f64], grad: &mut [f64]) {
    let nt = dot(theta, theta).min(BOUNDARY * BOUNDARY);
    let nx = dot(x, x).min(BOUNDARY * BOUNDARY);
    let alpha = 1.0 - nt;
    let beta = 1.0 - nx;
    let diff: f64 = theta.iter().zip(x).map(|(a, b)| (a - b) * (a - b)).sum();
    let gamma = 1.0 + 2.0 / (alpha * beta) * diff;
    let scale = 4.0 / (beta * (gamma * gamma - 1.0).max(EPS).sqrt());
    let coef = (nx - 2.0 * dot(theta, x) + 1.0) / (alpha * alpha);
    for k in 0..theta.len() {
        grad[k] += scale * (coef * theta[k] - x[k] / alpha);
    }
}

/// Train a Poincaré embedding over `n_nodes` with directed `edges` (`(child,
/// parent)` ⇒ child should be near parent). `dim` ball dimension, `epochs`,
/// `lr` learning rate, `neg_k` negatives per positive, `seed`.
pub fn train(
    n_nodes: usize,
    edges: &[(usize, usize)],
    dim: usize,
    epochs: usize,
    lr: f64,
    neg_k: usize,
    seed: u64,
) -> PoincareModel {
    let mut rng = XorShift64::new(seed);
    let mut emb: Vec<Vec<f64>> = (0..n_nodes)
        .map(|_| (0..dim).map(|_| rng.uniform(1e-3)).collect())
        .collect();
    if n_nodes < 2 || edges.is_empty() {
        return PoincareModel { dim, emb };
    }

    for _ in 0..epochs {
        for &(c, p) in edges {
            // Candidate set: the true parent + `neg_k` random negatives.
            let mut cands: Vec<usize> = Vec::with_capacity(neg_k + 1);
            cands.push(p);
            for _ in 0..neg_k {
                let mut q = rng.below(n_nodes);
                if q == c {
                    q = (q + 1) % n_nodes;
                }
                cands.push(q);
            }
            // Softmax over -distance; the positive (index 0) target prob 1.
            let dists: Vec<f64> = cands.iter().map(|&q| poincare_distance(&emb[c], &emb[q])).collect();
            let max_neg = dists.iter().cloned().fold(f64::MIN, f64::max);
            let exps: Vec<f64> = dists.iter().map(|d| (-(d - max_neg)).exp()).collect();
            let z: f64 = exps.iter().sum::<f64>().max(EPS);

            // dL/dd_q = softmax_q - 1{q == positive}; accumulate the child's grad.
            let mut grad_c = vec![0.0f64; dim];
            for (idx, &q) in cands.iter().enumerate() {
                let coeff = exps[idx] / z - if idx == 0 { 1.0 } else { 0.0 };
                if coeff.abs() < 1e-12 {
                    continue;
                }
                // grad of d(c,q) wrt c, and wrt q.
                let mut g_c = vec![0.0f64; dim];
                dist_grad(&emb[c], &emb[q], &mut g_c);
                let mut g_q = vec![0.0f64; dim];
                dist_grad(&emb[q], &emb[c], &mut g_q);
                // Riemannian rescale for q's own update: ((1-||q||^2)^2)/4.
                let aq = 1.0 - dot(&emb[q], &emb[q]).min(BOUNDARY * BOUNDARY);
                let rscale_q = aq * aq / 4.0;
                // coeff = p_q - 1{q=0} = -∂L/∂s_q; gradient *descent* on L moves
                // each point by +lr·rscale·coeff·(∂d/∂point) (the double negation).
                for k in 0..dim {
                    grad_c[k] += coeff * g_c[k];
                    emb[q][k] += lr * coeff * rscale_q * g_q[k];
                }
                project(&mut emb[q]);
            }
            // Riemannian rescale + gradient-descent update for the child.
            let ac = 1.0 - dot(&emb[c], &emb[c]).min(BOUNDARY * BOUNDARY);
            let rscale_c = ac * ac / 4.0;
            for k in 0..dim {
                emb[c][k] += lr * rscale_c * grad_c[k];
            }
            project(&mut emb[c]);
        }
    }
    PoincareModel { dim, emb }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn distance_is_symmetric_and_zero_on_self() {
        let m = PoincareModel {
            dim: 2,
            emb: vec![vec![0.1, 0.2], vec![-0.3, 0.05]],
        };
        assert!((m.distance(0, 1) - m.distance(1, 0)).abs() < 1e-9);
        assert!(m.distance(0, 0).abs() < 1e-6);
        assert!(m.distance(0, 1) > 0.0);
    }

    /// Contrastive training pulls `is_a`-connected pairs closer than
    /// unconnected cross-subtree pairs (the core Poincaré property). Robust to
    /// exact geometry; deterministic via the seed.
    #[test]
    fn training_makes_connected_pairs_closer() {
        // 0 = root; 1,2 children of 0; 3,4 children of 1; 5,6 children of 2.
        let edges = vec![(1, 0), (2, 0), (3, 1), (4, 1), (5, 2), (6, 2)];
        let m = train(7, &edges, 5, 500, 0.3, 5, 42);

        let connected: f64 =
            edges.iter().map(|(a, b)| m.distance(*a, *b)).sum::<f64>() / edges.len() as f64;
        // Cross-subtree leaf pairs that are NOT is_a-connected.
        let cross = [(3usize, 5usize), (3, 6), (4, 5), (4, 6)];
        let unconnected: f64 =
            cross.iter().map(|(a, b)| m.distance(*a, *b)).sum::<f64>() / cross.len() as f64;
        assert!(
            connected < unconnected,
            "is_a-connected mean distance {connected} must be < cross-subtree mean {unconnected}"
        );
    }

    /// Predicted edges are strictly norm-ordered ⇒ acyclic, and never duplicate an
    /// existing edge.
    #[test]
    fn predictions_are_acyclic_and_novel() {
        let edges = vec![(1, 0), (2, 0), (3, 1), (4, 1)];
        let m = train(5, &edges, 5, 300, 0.2, 4, 7);
        let existing: HashSet<(usize, usize)> = edges.iter().copied().collect();
        let preds = m.predict_missing(&existing, 1e9, 16);
        for (c, p, _) in &preds {
            assert!(!existing.contains(&(*c, *p)), "prediction must be novel");
            assert!(m.norm(*c) > m.norm(*p), "child strictly deeper ⇒ acyclic");
        }
        // No predicted (a,b) has its reverse (b,a) also predicted (strict order).
        let set: HashSet<(usize, usize)> = preds.iter().map(|(c, p, _)| (*c, *p)).collect();
        for (c, p) in &set {
            assert!(!set.contains(&(*p, *c)), "no symmetric pair ⇒ acyclic");
        }
    }
}
