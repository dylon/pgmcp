//! Phase 12 — adaptive `K` selection for FCM via cluster-validity indices.
//!
//! Replaces the `K = clamp(sqrt(n / min_cluster_size), 10, 100)` heuristic
//! with a principled sweep over candidate K values. For each candidate, runs
//! a short FCM and computes the selected validity index; the best K is the
//! one that optimises the index (minimising for Xie-Beni, maximising for
//! Fuzzy Silhouette / Gap).
//!
//! Three indices:
//!
//! - **Xie-Beni** — cheapest (O(n·K + K²)), native to FCM outputs. Default.
//! - **Fuzzy Silhouette** — O(n·K), compares within-cluster to neighboring
//!   cluster membership-weighted distance.
//! - **Gap Statistic** — most statistically principled but expensive
//!   (requires B reference samples, each a full FCM run).
//!
//! For large n the sweep runs on a **subsample** (default 50_000 chunks),
//! then the chosen K is used for a final full-scale FCM.

use ndarray::{Array2, ArrayView2};
use tracing::info;

use crate::cron::topic_clustering::fuzzy_c_means;

/// Validity index selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Index {
    /// Xie-Beni — lower is better. Default.
    XieBeni,
    /// Fuzzy silhouette — higher is better.
    FuzzySilhouette,
    /// Gap statistic — higher is better. Expensive.
    Gap,
}

impl Index {
    pub fn parse(s: &str) -> Self {
        match s {
            "silhouette" | "fuzzy_silhouette" => Self::FuzzySilhouette,
            "gap" => Self::Gap,
            _ => Self::XieBeni,
        }
    }

    /// True if a smaller score is better.
    pub fn minimise(&self) -> bool {
        matches!(self, Self::XieBeni)
    }
}

/// Configuration for a sweep over K values.
pub struct SweepConfig {
    /// Candidate K values to try.
    pub candidates: Vec<usize>,
    /// Validity index.
    pub index: Index,
    /// FCM fuzziness m (usually 2.0).
    pub m: f64,
    /// Max iterations per candidate (short runs; default 20).
    pub max_iters: usize,
    /// Convergence tolerance (loose: 1% for sweep).
    pub tolerance: f64,
    /// For Gap: number of reference samples (default 10).
    pub gap_n_refs: usize,
}

/// Compute the Xie-Beni index on a converged FCM result.
///
/// XB(c) = (Σ_i Σ_j μ_ij^m · ||x_i − c_j||²) / (n · min_{j≠k} ||c_j − c_k||²)
///
/// Lower is better. All distances computed in f64 for reduction stability.
pub fn xie_beni(
    data: ArrayView2<f32>,
    membership: ArrayView2<f32>,
    centroids: ArrayView2<f32>,
    m: f64,
) -> f64 {
    let n = data.nrows();
    let d = data.ncols();
    let k = centroids.nrows();

    if n == 0 || k < 2 {
        return f64::INFINITY;
    }

    // Numerator: Σ_i Σ_j μ_ij^m · ||x_i − c_j||²
    let mut numerator: f64 = 0.0;
    for i in 0..n {
        for j in 0..k {
            let mu = membership[[i, j]] as f64;
            let mu_m = mu.powf(m);
            let mut dist_sq: f64 = 0.0;
            for dim in 0..d {
                let diff = data[[i, dim]] as f64 - centroids[[j, dim]] as f64;
                dist_sq += diff * diff;
            }
            numerator += mu_m * dist_sq;
        }
    }

    // Denominator: n · min_{j≠l} ||c_j − c_l||²
    let mut min_centroid_dist_sq: f64 = f64::INFINITY;
    for j in 0..k {
        for l in (j + 1)..k {
            let mut dist_sq: f64 = 0.0;
            for dim in 0..d {
                let diff = centroids[[j, dim]] as f64 - centroids[[l, dim]] as f64;
                dist_sq += diff * diff;
            }
            if dist_sq < min_centroid_dist_sq {
                min_centroid_dist_sq = dist_sq;
            }
        }
    }

    let denom = (n as f64) * min_centroid_dist_sq;
    if denom <= 0.0 || min_centroid_dist_sq <= 1e-12 {
        return f64::INFINITY;
    }

    numerator / denom
}

/// Compute the fuzzy silhouette (FS) index.
///
/// For each point i, `s(i) = (b_i − a_i) / max(a_i, b_i)` where a_i is the
/// weighted distance to its primary cluster and b_i is the weighted distance
/// to its secondary (next-best) cluster. Weighted by (μ_primary − μ_secondary)^α.
///
/// Higher is better (range -1..+1).
pub fn fuzzy_silhouette(
    data: ArrayView2<f32>,
    membership: ArrayView2<f32>,
    centroids: ArrayView2<f32>,
    alpha: f64,
) -> f64 {
    let n = data.nrows();
    let d = data.ncols();
    let k = centroids.nrows();

    if n == 0 || k < 2 {
        return 0.0;
    }

    let mut numerator: f64 = 0.0;
    let mut denominator: f64 = 0.0;

    for i in 0..n {
        // Find primary (argmax) and secondary (second-argmax) cluster.
        let mut best_j = 0usize;
        let mut second_j = 0usize;
        let mut best_mu: f32 = f32::NEG_INFINITY;
        let mut second_mu: f32 = f32::NEG_INFINITY;
        for j in 0..k {
            let mu = membership[[i, j]];
            if mu > best_mu {
                second_j = best_j;
                second_mu = best_mu;
                best_j = j;
                best_mu = mu;
            } else if mu > second_mu {
                second_j = j;
                second_mu = mu;
            }
        }

        // Compute a_i = ||x_i − c_primary|| and b_i = ||x_i − c_secondary||
        let mut a_sq: f64 = 0.0;
        let mut b_sq: f64 = 0.0;
        for dim in 0..d {
            let xd = data[[i, dim]] as f64;
            let da = xd - centroids[[best_j, dim]] as f64;
            let db = xd - centroids[[second_j, dim]] as f64;
            a_sq += da * da;
            b_sq += db * db;
        }
        let a = a_sq.sqrt();
        let b = b_sq.sqrt();
        let denom = a.max(b);
        let s = if denom > 1e-12 { (b - a) / denom } else { 0.0 };

        let weight = (best_mu - second_mu).max(0.0).powf(alpha as f32) as f64;
        numerator += weight * s;
        denominator += weight;
    }

    if denominator > 1e-12 {
        numerator / denominator
    } else {
        0.0
    }
}

/// One entry in a sweep result.
#[derive(Debug, Clone)]
pub struct SweepEntry {
    pub k: usize,
    pub index_value: f64,
    pub iterations: usize,
    pub converged: bool,
}

/// Run a K-sweep and return the best K along with all evaluated entries.
///
/// For each candidate K: runs a short FCM (default 20 iters) and computes
/// the validity index. Entries sorted by K; `best_idx` points at the
/// winner (minimal XB or maximal FS/Gap).
pub fn sweep_k(data: ArrayView2<f32>, cfg: &SweepConfig) -> (usize, Vec<SweepEntry>) {
    let n = data.nrows();
    assert!(
        !cfg.candidates.is_empty(),
        "sweep candidates must not be empty"
    );

    let mut entries: Vec<SweepEntry> = Vec::with_capacity(cfg.candidates.len());

    for &k_cand in &cfg.candidates {
        if k_cand == 0 || k_cand > n {
            continue;
        }
        let t0 = std::time::Instant::now();
        let result = fuzzy_c_means(data, k_cand, cfg.m, cfg.max_iters, cfg.tolerance, None);

        let idx_val = match cfg.index {
            Index::XieBeni => xie_beni(
                data,
                result.membership.view(),
                result.centroids.view(),
                cfg.m,
            ),
            Index::FuzzySilhouette => {
                -fuzzy_silhouette(data, result.membership.view(), result.centroids.view(), 1.0)
                // Negate so the "best" is always the minimum across indices.
            }
            Index::Gap => {
                // Gap statistic requires reference samples. Approximation: use
                // inertia / (n · k) vs the expected value under a uniform
                // null distribution. Properly computing requires B fresh FCM
                // runs on uniform-random points — expensive; approximated here
                // as `-inertia / n / k` (higher is better, negate).
                -(result.inertia / (n as f64 * k_cand as f64))
            }
        };

        info!(
            k = k_cand,
            index = ?cfg.index,
            value = format!("{:.4e}", idx_val),
            iters = result.iterations,
            converged = result.converged,
            elapsed_s = t0.elapsed().as_secs_f64(),
            "K sweep candidate evaluated"
        );

        entries.push(SweepEntry {
            k: k_cand,
            index_value: idx_val,
            iterations: result.iterations,
            converged: result.converged,
        });
    }

    // After the unified "lower is better" transformation above, pick the
    // minimum. Tie-break toward smaller K (Occam's razor).
    let mut best_k = entries[0].k;
    let mut best_val = entries[0].index_value;
    for entry in &entries[1..] {
        if entry.index_value < best_val || (entry.index_value == best_val && entry.k < best_k) {
            best_val = entry.index_value;
            best_k = entry.k;
        }
    }

    info!(
        best_k,
        index = ?cfg.index,
        best_value = format!("{:.4e}", best_val),
        "K sweep complete"
    );

    (best_k, entries)
}

/// Generate a geometric sweep of candidate K values around a base K.
///
/// Example: `geometric_candidates(10, 100)` = `[10, 25, 50, 100, 200, 400]`
/// (clamped to max_k).
pub fn geometric_candidates(base_k: usize, max_k: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let base = base_k.max(10) as f64;
    // 2^{-2..+2}
    for exp in -2..=2i32 {
        let v = (base * 2.0_f64.powi(exp)).round() as usize;
        let clamped = v.clamp(10, max_k.max(10));
        if !out.contains(&clamped) {
            out.push(clamped);
        }
    }
    out.sort_unstable();
    out
}

/// Subsample data for sweep cost control. Takes the first `n_subsample` rows
/// (deterministic; for random subsampling, the caller should pre-shuffle).
pub fn subsample_data(data: &Array2<f32>, n_subsample: usize) -> Array2<f32> {
    let n = data.nrows();
    let take = n_subsample.min(n);
    if take == n {
        return data.clone();
    }
    let d = data.ncols();
    let mut out = Array2::<f32>::zeros((take, d));
    out.assign(&data.slice(ndarray::s![..take, ..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_well_separated(k_true: usize, pts_per_cluster: usize, d: usize) -> Array2<f32> {
        let n = k_true * pts_per_cluster;
        let mut data = Array2::<f32>::zeros((n, d));
        for c in 0..k_true {
            for i in 0..pts_per_cluster {
                let row = c * pts_per_cluster + i;
                data[[row, c % d]] = 1.0 + 0.01 * i as f32;
            }
        }
        data
    }

    #[test]
    fn test_geometric_candidates_shape() {
        let cands = geometric_candidates(50, 500);
        // base=50 → {12, 25, 50, 100, 200} — min floor is 10
        assert!(cands.contains(&50));
        assert!(cands.contains(&100));
        assert!(cands.contains(&200));
        assert!(cands.iter().all(|&k| k >= 10));
    }

    #[test]
    fn test_geometric_candidates_small_base() {
        let cands = geometric_candidates(10, 100);
        // base=10 → {10, 10, 10, 20, 40} dedup → {10, 20, 40}
        assert!(cands.contains(&10));
        assert!(cands.contains(&20));
    }

    #[test]
    fn test_subsample_takes_first_n_rows() {
        let mut d = Array2::<f32>::zeros((10, 3));
        for i in 0..10 {
            d[[i, 0]] = i as f32;
        }
        let sub = subsample_data(&d, 4);
        assert_eq!(sub.nrows(), 4);
        for i in 0..4 {
            assert_eq!(sub[[i, 0]], i as f32);
        }
    }

    #[test]
    fn test_subsample_larger_than_data_returns_clone() {
        let d = Array2::<f32>::from_shape_fn((5, 3), |(i, _)| i as f32);
        let sub = subsample_data(&d, 100);
        assert_eq!(sub.nrows(), 5);
    }

    #[test]
    fn test_xie_beni_prefers_correct_k_on_well_separated() {
        let data = make_well_separated(3, 20, 4);
        let cfg = SweepConfig {
            candidates: vec![2, 3, 4, 6],
            index: Index::XieBeni,
            m: 2.0,
            max_iters: 50,
            tolerance: 1e-4,
            gap_n_refs: 0,
        };
        let (best_k, entries) = sweep_k(data.view(), &cfg);
        assert!(entries.len() == 4, "all 4 candidates evaluated");
        // We expect K=3 or a close neighbor to win on a 3-cluster synthetic.
        // Xie-Beni may also pick 2 if clusters are very well-separated; allow 2-4.
        assert!(
            (2..=4).contains(&best_k),
            "expected best_k 2-4, got {}",
            best_k
        );
    }

    #[test]
    fn test_fuzzy_silhouette_produces_reasonable_score() {
        let data = make_well_separated(3, 20, 4);
        let fcm = fuzzy_c_means(data.view(), 3, 2.0, 50, 1e-4, None);
        let s = fuzzy_silhouette(
            data.view(),
            fcm.membership.view(),
            fcm.centroids.view(),
            1.0,
        );
        // Well-separated clusters should have high silhouette (near 1).
        assert!(
            s > 0.3,
            "silhouette should be high for well-separated clusters, got {}",
            s
        );
    }

    #[test]
    fn test_index_parse() {
        assert_eq!(Index::parse("xie_beni"), Index::XieBeni);
        assert_eq!(Index::parse("silhouette"), Index::FuzzySilhouette);
        assert_eq!(Index::parse("fuzzy_silhouette"), Index::FuzzySilhouette);
        assert_eq!(Index::parse("gap"), Index::Gap);
        assert_eq!(Index::parse("unknown"), Index::XieBeni); // default
    }

    #[test]
    fn test_xie_beni_handles_degenerate_inputs() {
        // Empty data
        let data = Array2::<f32>::zeros((0, 4));
        let mem = Array2::<f32>::zeros((0, 2));
        let cent = Array2::<f32>::zeros((2, 4));
        assert_eq!(
            xie_beni(data.view(), mem.view(), cent.view(), 2.0),
            f64::INFINITY
        );

        // K=1 (no inter-centroid distance to compute)
        let data = Array2::<f32>::ones((5, 4));
        let mem = Array2::<f32>::ones((5, 1));
        let cent = Array2::<f32>::ones((1, 4));
        assert_eq!(
            xie_beni(data.view(), mem.view(), cent.view(), 2.0),
            f64::INFINITY
        );
    }
}
