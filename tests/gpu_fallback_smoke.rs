//! CUDA-mandatory smoke test for the CUDA dispatch path.
//!
//! Sets `CUDA_VISIBLE_DEVICES=""` before any `CudaContext::new()` runs,
//! which forces the cudarc runtime probe to fail. The `GpuFcm*::new()`
//! constructor then returns `Err`, and `fuzzy_c_means_gpu` returns a
//! degenerate result instead of silently falling back to CPU.
//!
//! Run with:
//!   cargo test --release --test gpu_fallback_smoke -- --ignored
//!
//! This test is `#[ignore]` by default because:
//!   (a) it mutates process-wide CUDA runtime state, so must run in an
//!       isolated test binary;
//!   (b) this binary mutates CUDA visibility to prove the production path
//!       fails closed even on a GPU host.

use ndarray::Array2;

#[test]
#[ignore = "requires CUDA feature; forces GPU-unavailable to exercise fail-closed behavior"]
fn gpu_init_failure_returns_degenerate_result() {
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

    assert_eq!(result.iterations, 0);
    assert!(!result.converged);
    assert_eq!(result.membership.nrows(), 2000);
    assert_eq!(result.membership.ncols(), 2);
    assert!(
        result.membership.iter().all(|&v| v == 0.0),
        "mandatory CUDA failure should not synthesize CPU memberships"
    );
}

/// Same fail-closed path, but driven with deterministic 384-dim "realistic"
/// embeddings (matching production vector dimensionality).
#[test]
#[ignore = "requires CUDA feature; forces GPU-unavailable with real-width vectors"]
fn gpu_init_failure_with_realistic_embeddings_returns_degenerate_result() {
    // SAFETY: single-threaded in this test binary before CUDA init.
    unsafe {
        std::env::set_var("CUDA_VISIBLE_DEVICES", "");
    }
    let dim = 384;
    let per_cluster = 200;
    let k_true = 3;
    let n = per_cluster * k_true;
    let mut data = Array2::<f32>::zeros((n, dim));
    // Hash-seeded deterministic offsets (same logic as
    // `DeterministicEmbeddingBackend`): each cluster uses a distinct
    // per-dim offset so the blobs are linearly separable in 384-D.
    for c in 0..k_true {
        for i in 0..per_cluster {
            let row = c * per_cluster + i;
            for d in 0..dim {
                let phase = ((c * 31 + d) as f32) * 0.01;
                data[[row, d]] = phase + (i as f32) * 1e-4;
            }
        }
    }
    let result = pgmcp::cron::topic_clustering::fuzzy_c_means_gpu(
        data.view(),
        k_true,
        2.0,
        50,
        1e-4,
        pgmcp::fcm::GpuPrecision::Fp16,
    );
    assert_eq!(result.iterations, 0);
    assert!(!result.converged);
    assert_eq!(result.membership.nrows(), n);
    assert_eq!(result.membership.ncols(), k_true);
    assert!(result.membership.iter().all(|&v| v == 0.0));
}
