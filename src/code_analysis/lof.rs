//! True Local Outlier Factor (Breunig, Kriegel, Ng & Sander, "LOF: Identifying
//! Density-Based Local Outliers", SIGMOD 2000). (graph-roadmap Phase 3.5)
//!
//! The prior `embedding_outliers` used an *approximate* LOF — a point's mean
//! k-NN distance ÷ the global mean. That flags globally-distant points but
//! misses the actual LOF insight: an outlier is a point whose *local
//! reachability density* is much lower than its neighbors' densities, so a
//! point on the fringe of a tight cluster is caught even when its absolute
//! distance is unremarkable. This module computes the real definition:
//!
//! - `k-distance(o)` = distance to `o`'s k-th nearest neighbor,
//! - `reach-dist_k(p, o)` = max(k-distance(o), d(p, o)),
//! - `lrd_k(p)` = 1 / mean_{o ∈ N_k(p)} reach-dist_k(p, o),
//! - `LOF_k(p)` = mean_{o ∈ N_k(p)} lrd_k(o) / lrd_k(p).
//!
//! LOF ≈ 1 ⇒ density matches the neighborhood (inlier); LOF ≫ 1 ⇒ much sparser
//! than neighbors (outlier). Pure: the caller supplies the k-NN lists (from the
//! HNSW index); no DB or model here.

/// Compute the LOF of every point from its k-NN lists.
///
/// `neighbors[i]` is point `i`'s nearest neighbors as `(point_index, distance)`
/// ascending by distance (length ≥ 0; the first `k` are used). `point_index`
/// must index back into `neighbors`. Returns one LOF per point (1.0 for points
/// with no neighbors).
pub fn local_outlier_factors(neighbors: &[Vec<(usize, f64)>], k: usize) -> Vec<f64> {
    let n = neighbors.len();
    let k = k.max(1);
    if n == 0 {
        return Vec::new();
    }

    // k-distance(o): distance to the k-th neighbor (or the farthest available).
    let k_distance: Vec<f64> = neighbors
        .iter()
        .map(|nn| {
            if nn.is_empty() {
                0.0
            } else {
                nn[nn.len().min(k) - 1].1
            }
        })
        .collect();

    // Local reachability density. Clamp the mean reach-distance away from zero
    // so duplicate/degenerate points yield a large-but-finite density rather
    // than an infinity that would poison the ratio below.
    let lrd: Vec<f64> = (0..n)
        .map(|i| {
            let used = &neighbors[i][..neighbors[i].len().min(k)];
            if used.is_empty() {
                return 0.0;
            }
            let sum: f64 = used
                .iter()
                .map(|&(o, d)| k_distance.get(o).copied().unwrap_or(d).max(d))
                .sum();
            let mean = (sum / used.len() as f64).max(1e-12);
            1.0 / mean
        })
        .collect();

    (0..n)
        .map(|i| {
            let used = &neighbors[i][..neighbors[i].len().min(k)];
            if used.is_empty() || lrd[i] <= 0.0 {
                return 1.0;
            }
            let sum: f64 = used
                .iter()
                .map(|&(o, _)| lrd.get(o).copied().unwrap_or(0.0) / lrd[i])
                .sum();
            sum / used.len() as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Five points: 0..=3 form a tight cluster (mutual distance ~1), point 4 is
    // far (distance ~10). Build symmetric k-NN lists (k=2).
    fn clustered_knn() -> Vec<Vec<(usize, f64)>> {
        vec![
            vec![(1, 1.0), (2, 1.0), (3, 1.4), (4, 10.0)],   // 0
            vec![(0, 1.0), (2, 1.0), (3, 1.0), (4, 10.0)],   // 1
            vec![(1, 1.0), (0, 1.0), (3, 1.0), (4, 10.0)],   // 2
            vec![(1, 1.0), (2, 1.0), (0, 1.4), (4, 9.5)],    // 3
            vec![(3, 9.5), (2, 10.0), (1, 10.0), (0, 10.0)], // 4 — the outlier
        ]
    }

    #[test]
    fn outlier_has_high_lof_inliers_near_one() {
        let lof = local_outlier_factors(&clustered_knn(), 2);
        assert_eq!(lof.len(), 5);
        // Cluster members hover near 1.0.
        for (i, &v) in lof.iter().enumerate().take(4) {
            assert!(v < 2.0, "inlier {i} LOF should be ~1, got {v}");
        }
        // The far point is markedly higher than every inlier.
        let max_inlier = lof[..4].iter().cloned().fold(0.0_f64, f64::max);
        assert!(
            lof[4] > max_inlier * 1.5,
            "outlier LOF {} should dominate inliers (max {})",
            lof[4],
            max_inlier
        );
    }

    #[test]
    fn uniform_points_are_near_one() {
        // All mutual distances equal ⇒ every density equal ⇒ LOF == 1.
        let nn = vec![
            vec![(1, 2.0), (2, 2.0)],
            vec![(0, 2.0), (2, 2.0)],
            vec![(0, 2.0), (1, 2.0)],
        ];
        let lof = local_outlier_factors(&nn, 2);
        for v in lof {
            assert!((v - 1.0).abs() < 1e-9, "uniform LOF should be 1, got {v}");
        }
    }

    #[test]
    fn empty_and_singletons_are_safe() {
        assert!(local_outlier_factors(&[], 3).is_empty());
        assert_eq!(local_outlier_factors(&[vec![]], 3), vec![1.0]);
    }
}
