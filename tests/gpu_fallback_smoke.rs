//! Fallback-to-CPU smoke test for the cuda dispatch path.
//!
//! Sets `CUDA_VISIBLE_DEVICES=""` before any `CudaContext::new()` runs,
//! which forces the cudarc runtime probe to fail. The `GpuFcm*::new()`
//! constructor then returns `Err`, and `fuzzy_c_means_gpu` routes through
//! its existing WARN + CPU fallback. This verifies the fallback path
//! produces a valid (converged) FCM result.
//!
//! Run with:
//!   cargo test --release --test gpu_fallback_smoke -- --ignored
//!
//! This test is `#[ignore]` by default because:
//!   (a) it mutates process-wide CUDA runtime state, so must run in an
//!       isolated test binary;
//!   (b) on a GPU-less host the fallback path is exercised by every
//!       test — this binary's job is to prove it works on a GPU host too.

use ndarray::Array2;

#[test]
#[ignore = "requires CUDA feature; forces GPU-unavailable to exercise fallback"]
fn gpu_init_failure_falls_back_to_cpu() {
    // SAFETY: This must run before ANY CudaContext::new() call is made in
    // this process. Because `tests/gpu_fallback_smoke.rs` is a standalone
    // test binary with a single `#[test]`, this is always the first CUDA
    // interaction in the process.
    //
    // `set_var` in a multi-threaded process is technically unsound (env
    // reads are not atomic with writes), but Rust test binaries run each
    // test in its own thread spawned after main starts and there is no
    // other CUDA work on this binary's critical path.
    // SAFETY: single-threaded in this test binary before CUDA init.
    unsafe {
        std::env::set_var("CUDA_VISIBLE_DEVICES", "");
    }

    let mut data = Array2::<f32>::zeros((2000, 384));
    // Two trivially separable blobs so we can detect a real FCM result.
    for i in 0..1000 {
        data[[i, 0]] = 1.0;
    }
    for i in 1000..2000 {
        data[[i, 1]] = 1.0;
    }

    let result = pgmcp::cron::topic_clustering::fuzzy_c_means_gpu(
        data.view(),
        2,
        2.0,
        50,
        1e-4,
        pgmcp::fcm::GpuPrecision::Fp16,
    );

    assert!(
        result.iterations > 0,
        "fallback path did not run (iters = 0)"
    );
    assert_eq!(result.membership.nrows(), 2000);
    assert_eq!(result.membership.ncols(), 2);

    // Centroids should reflect the two blobs even from the CPU fallback.
    let c0 = result.centroids.row(0);
    let c1 = result.centroids.row(1);
    let dist_sq: f32 = c0.iter().zip(c1.iter()).map(|(a, b)| (a - b).powi(2)).sum();
    assert!(
        dist_sq.sqrt() > 0.5,
        "fallback centroids are collapsed: dist = {}",
        dist_sq.sqrt()
    );
}
