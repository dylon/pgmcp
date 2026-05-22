//! Regression test for Phase 5 (GPU fp16 Tensor-Core path).
//!
//! Compares converged centroids between `BackendChoice::Cuda(Fp32)` and
//! `BackendChoice::Cuda(Fp16)` on a synthetic normalized dataset. The
//! two paths must agree within a tolerance dominated by fp16's ~3-4
//! significant-decimal-digit precision.
//!
//! Self-skips when CUDA is unavailable (no GPU, no `nvcc`, runtime
//! init failure). The skip is via `BackendChoice::Cuda(...)` returning
//! `Err(FcmError::Cuda(_))`; the test then exits Ok rather than failing
//! a CI box without a GPU.

use ndarray::Array2;
use pgmcp::fcm::{BackendChoice, FcmError, GpuPrecision, make_backend, run_seeded};

fn build_normalized_blobs(n_per_cluster: usize, d: usize, k_true: usize) -> Array2<f32> {
    let n = n_per_cluster * k_true;
    let mut data = Array2::<f32>::zeros((n, d));

    // Deterministic LCG so the test is reproducible.
    let mut s: u64 = 0xDEADBEEF_CAFEBABE;
    let mut next_f = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((s >> 33) as f32 / (1u32 << 31) as f32) - 0.5 // ~[-0.5, +0.5]
    };

    for c in 0..k_true {
        // Centre vector with a single dominant dimension per cluster.
        let mut centre = vec![0.0f32; d];
        centre[c % d] = 1.0;

        for i in 0..n_per_cluster {
            let row = c * n_per_cluster + i;
            for dim in 0..d {
                let noise = 0.05 * next_f();
                data[[row, dim]] = centre[dim] + noise;
            }
            // L2-normalize each row so |v| ≤ 1; fp16 stays well within
            // its precision envelope.
            let norm: f32 = (0..d)
                .map(|j| data[[row, j]] * data[[row, j]])
                .sum::<f32>()
                .sqrt();
            if norm > 1e-12 {
                for j in 0..d {
                    data[[row, j]] /= norm;
                }
            }
        }
    }
    data
}

fn try_run_one(
    data: Array2<f32>,
    k: usize,
    precision: GpuPrecision,
) -> Result<Option<Array2<f32>>, FcmError> {
    match make_backend(data.clone(), k, BackendChoice::Cuda(precision)) {
        Ok(mut backend) => {
            let result = run_seeded(
                backend.as_mut(),
                data.view(),
                k,
                2.0,
                100,
                1e-4,
                None,
                None,
                Some(0xC0FFEE),
            )?;
            Ok(Some(result.centroids))
        }
        // CUDA unavailable at construction time — the canonical signal
        // for "skip this test on a CPU-only host". `CudaInit` is
        // construction-time only; `CudaLaunch` is a runtime kernel
        // failure (treat as a real test failure rather than skip).
        Err(FcmError::CudaInit(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

#[test]
fn fp16_vs_fp32_centroid_divergence_below_threshold() {
    let data = build_normalized_blobs(40, 8, 3);
    let k = 3;

    let fp32 = match try_run_one(data.clone(), k, GpuPrecision::Fp32).expect("fp32 errored") {
        Some(c) => c,
        None => {
            eprintln!("CUDA unavailable; skipping fp16-vs-fp32 convergence test");
            return;
        }
    };
    let fp16 = match try_run_one(data.clone(), k, GpuPrecision::Fp16).expect("fp16 errored") {
        Some(c) => c,
        None => {
            eprintln!("CUDA unavailable; skipping fp16-vs-fp32 convergence test");
            return;
        }
    };

    // Centroid arrays may come back in different cluster orderings
    // (k-means++ seeds determine the labelling). For each fp32 row,
    // find its closest fp16 row and assert distance ≤ tolerance.
    let mut max_min_dist = 0.0_f32;
    for i in 0..k {
        let mut best = f32::MAX;
        for j in 0..k {
            let mut s = 0.0_f32;
            for d in 0..fp32.ncols() {
                let diff = fp32[[i, d]] - fp16[[j, d]];
                s += diff * diff;
            }
            if s < best {
                best = s;
            }
        }
        let dist = best.sqrt();
        if dist > max_min_dist {
            max_min_dist = dist;
        }
    }

    // ~3-4 decimal digits of agreement is the published fp16-vs-fp32
    // contract for FCM on normalized data. A 5e-2 tolerance gives
    // headroom against k-means++ seed sensitivity at the chosen seed
    // without becoming a noise filter.
    assert!(
        max_min_dist <= 5.0e-2,
        "fp16 vs fp32 centroid max-min-distance is {max_min_dist}, expected <= 5e-2"
    );
}
