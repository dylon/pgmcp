//! Golden-file tests for `pgmcp::cron::topic_clustering::fuzzy_c_means_seeded`.
//!
//! FCM output is `FcmResult` — two `f32` matrices (membership n×K and
//! centroids K×d) plus a few scalar fields. We allow up to 1e-5
//! per-cell drift on the matrices (CPU GEMM accumulation is not
//! bit-stable across builds), but require exact equality on the
//! discrete fields (`iterations`, `converged`, `cancelled`) and a
//! tighter 1e-4 tolerance on the f64 `inertia` accumulator.

use ndarray::{Array2, ArrayView2};
use pgmcp::cron::topic_clustering;
use pgmcp::fcm::FcmResult;
use pgmcp_testing::golden::assert_match_epsilon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FcmInput {
    data: Array2<f32>,
    k: usize,
    fuzziness: f64,
    max_iters: usize,
    tolerance: f64,
    seed: u64,
}

fn run(input: &FcmInput) -> FcmResult {
    topic_clustering::fuzzy_c_means_seeded(
        ArrayView2::from(&input.data),
        input.k,
        input.fuzziness,
        input.max_iters,
        input.tolerance,
        input.seed,
    )
}

/// Worst per-cell error across membership and centroid matrices, plus
/// a discrete check on the scalar fields. Discrete mismatches return
/// `f64::INFINITY` so the assertion never lies. Note that FCM
/// permutes cluster indices freely — we resolve permutation by greedy
/// matching centroids by closest-pair before comparing rows.
fn max_fcm_error(expected: &FcmResult, actual: &FcmResult) -> f64 {
    if expected.membership.dim() != actual.membership.dim()
        || expected.centroids.dim() != actual.centroids.dim()
    {
        return f64::INFINITY;
    }
    if expected.iterations != actual.iterations
        || expected.converged != actual.converged
        || expected.cancelled != actual.cancelled
    {
        return f64::INFINITY;
    }

    // Permutation alignment: build a K-element vector mapping
    // expected-cluster-index → actual-cluster-index by nearest centroid.
    let k = expected.centroids.nrows();
    let mut perm: Vec<usize> = vec![usize::MAX; k];
    let mut used: Vec<bool> = vec![false; k];
    for (ei, slot) in perm.iter_mut().enumerate().take(k) {
        let mut best_aj = usize::MAX;
        let mut best_d = f32::INFINITY;
        for (aj, already_used) in used.iter().enumerate().take(k) {
            if *already_used {
                continue;
            }
            let mut d2: f32 = 0.0;
            for col in 0..expected.centroids.ncols() {
                let diff = expected.centroids[[ei, col]] - actual.centroids[[aj, col]];
                d2 += diff * diff;
            }
            if d2 < best_d {
                best_d = d2;
                best_aj = aj;
            }
        }
        if best_aj == usize::MAX {
            return f64::INFINITY;
        }
        *slot = best_aj;
        used[best_aj] = true;
    }

    let mut worst: f64 = 0.0;
    for (ei, aj) in perm.iter().copied().enumerate().take(k) {
        for col in 0..expected.centroids.ncols() {
            let d = (expected.centroids[[ei, col]] - actual.centroids[[aj, col]]).abs() as f64;
            if d > worst {
                worst = d;
            }
        }
        for row in 0..expected.membership.nrows() {
            let d = (expected.membership[[row, ei]] - actual.membership[[row, aj]]).abs() as f64;
            if d > worst {
                worst = d;
            }
        }
    }
    // Inertia drift gets folded into the same tolerance — for the
    // two-blob fixture inertia is on the order of 0.1, so 1e-5
    // dominates and the inertia error stays well within budget.
    let inertia_err = (expected.inertia - actual.inertia).abs();
    if inertia_err > worst {
        worst = inertia_err;
    }
    worst
}

#[test]
fn two_blobs_seed_42_matches_golden() {
    assert_match_epsilon::<FcmInput, FcmResult>("fcm/two_blobs_seed_42", run, max_fcm_error);
}
