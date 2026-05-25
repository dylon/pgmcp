//! Isolation Forest (Liu, Ting & Zhou, "Isolation Forest", ICDM 2008).
//! (graph-roadmap Phase 3.5)
//!
//! `anomaly_detection` advertised a `contamination` parameter but scored points
//! with a hand-weighted sum of z-scores. This implements the real isolation
//! forest the parameter implies: anomalies are points that are *easy to
//! isolate* — a random axis-parallel split tree separates them from the bulk in
//! few cuts, so their expected path length is short. The anomaly score is
//! `s(x) = 2^(-E[h(x)] / c(ψ))`, in (0, 1): ≈1 ⇒ anomaly, ≪0.5 ⇒ normal.
//!
//! Pure + dependency-free: a small SplitMix64 RNG keeps the forest deterministic
//! given a seed (no `rand` dependency), so results are reproducible.

/// Deterministic SplitMix64 — tiny, seedable, good enough for split selection.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform f64 in [0, 1).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

/// An isolation-tree node: internal (split on a feature) or external (leaf).
enum Node {
    Internal {
        feature: usize,
        split: f64,
        left: Box<Node>,
        right: Box<Node>,
    },
    External {
        size: usize,
    },
}

/// Average path length of an unsuccessful BST search over `n` points — the
/// normalization constant c(n) from the paper.
fn c_factor(n: usize) -> f64 {
    if n <= 1 {
        return 0.0;
    }
    let n = n as f64;
    2.0 * ((n - 1.0).ln() + 0.577_215_664_901_532_9) - 2.0 * (n - 1.0) / n
}

fn build_tree(
    data: &[Vec<f64>],
    indices: &[usize],
    depth: usize,
    max_depth: usize,
    n_features: usize,
    rng: &mut SplitMix64,
) -> Node {
    if depth >= max_depth || indices.len() <= 1 || n_features == 0 {
        return Node::External {
            size: indices.len(),
        };
    }
    // Random feature, random split in [min, max] of that feature over `indices`.
    let feature = rng.below(n_features);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &i in indices {
        let v = data[i].get(feature).copied().unwrap_or(0.0);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if hi <= lo {
        // Degenerate (constant feature over this subset) — can't split here.
        // (lo/hi are finite min/max over the subset, so a direct `<=` is exact.)
        return Node::External {
            size: indices.len(),
        };
    }
    let split = lo + rng.next_f64() * (hi - lo);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for &i in indices {
        let v = data[i].get(feature).copied().unwrap_or(0.0);
        if v < split {
            left.push(i);
        } else {
            right.push(i);
        }
    }
    if left.is_empty() || right.is_empty() {
        return Node::External {
            size: indices.len(),
        };
    }
    Node::Internal {
        feature,
        split,
        left: Box::new(build_tree(
            data,
            &left,
            depth + 1,
            max_depth,
            n_features,
            rng,
        )),
        right: Box::new(build_tree(
            data,
            &right,
            depth + 1,
            max_depth,
            n_features,
            rng,
        )),
    }
}

/// Path length of `x` in `tree`: edges traversed + c(leaf size) for the
/// unbuilt subtree below an early-terminated external node.
fn path_length(tree: &Node, x: &[f64], depth: usize) -> f64 {
    match tree {
        Node::External { size } => depth as f64 + c_factor(*size),
        Node::Internal {
            feature,
            split,
            left,
            right,
        } => {
            let v = x.get(*feature).copied().unwrap_or(0.0);
            if v < *split {
                path_length(left, x, depth + 1)
            } else {
                path_length(right, x, depth + 1)
            }
        }
    }
}

/// Fit an isolation forest and return each point's anomaly score in (0, 1].
///
/// - `n_trees`: number of isolation trees (default-ish 100).
/// - `sample_size`: subsample (ψ) per tree (paper default 256); clamped to ≤ n.
/// - `seed`: RNG seed for reproducibility.
pub fn anomaly_scores(
    data: &[Vec<f64>],
    n_trees: usize,
    sample_size: usize,
    seed: u64,
) -> Vec<f64> {
    let n = data.len();
    if n == 0 {
        return Vec::new();
    }
    let n_features = data.iter().map(|r| r.len()).max().unwrap_or(0);
    let psi = sample_size.clamp(1, n);
    let max_depth = ((psi as f64).log2().ceil() as usize).max(1);
    let norm = c_factor(psi).max(1e-12);
    let n_trees = n_trees.max(1);

    let mut rng = SplitMix64::new(seed);
    // Build the forest, each tree on a random subsample of size ψ.
    let mut trees: Vec<Node> = Vec::with_capacity(n_trees);
    for _ in 0..n_trees {
        let mut sample: Vec<usize> = Vec::with_capacity(psi);
        for _ in 0..psi {
            sample.push(rng.below(n));
        }
        trees.push(build_tree(
            data, &sample, 0, max_depth, n_features, &mut rng,
        ));
    }

    // Score every point: mean path length over the forest → s(x).
    (0..n)
        .map(|i| {
            let mean_h: f64 = trees
                .iter()
                .map(|t| path_length(t, &data[i], 0))
                .sum::<f64>()
                / n_trees as f64;
            2f64.powf(-mean_h / norm)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obvious_outlier_scores_highest() {
        // A tight 1-D cluster near 0 plus one point far away at 100.
        let mut data: Vec<Vec<f64>> = (0..30).map(|i| vec![(i as f64) * 0.01]).collect();
        data.push(vec![100.0]);
        let scores = anomaly_scores(&data, 100, 16, 42);
        assert_eq!(scores.len(), 31);
        let outlier = scores[30];
        let max_inlier = scores[..30].iter().cloned().fold(0.0_f64, f64::max);
        assert!(
            outlier > max_inlier,
            "far point ({outlier}) should out-score every cluster point (max {max_inlier})"
        );
    }

    #[test]
    fn deterministic_given_seed() {
        let data: Vec<Vec<f64>> = (0..20).map(|i| vec![i as f64, (i % 3) as f64]).collect();
        let a = anomaly_scores(&data, 50, 8, 7);
        let b = anomaly_scores(&data, 50, 8, 7);
        assert_eq!(a, b, "same seed must give identical scores");
    }

    #[test]
    fn empty_input_is_safe() {
        assert!(anomaly_scores(&[], 100, 256, 1).is_empty());
    }
}
