//! Fuzzy C-Means (FCM) backend abstraction.
//!
//! This module is the **swap seam** between compute backends. The primary
//! path is CUDA (cuBLAS GEMM + a nvcc-compiled fused post-GEMM reduction
//! kernel). The CPU backend exists only as a runtime fallback when CUDA
//! initialization fails. A future third backend (Metal, ROCm, …) would plug
//! in as a new `FcmBackend` impl without touching callers.
//!
//! ## Design choices
//!
//! - **Trait** (`FcmBackend`): behavior surface with real polymorphism
//!   candidates (today two, future more).
//! - **Enum** (`BackendChoice`): closed construction-time selector. Maps 1:1
//!   onto a concrete `FcmBackend` impl via `make_backend`.
//! - **Enum** (`GpuPrecision`): closed set of CUDA-precision choices
//!   (`Fp32 | Fp16 | Bf16`) consumed by the CUDA backend's constructor.
//! - **Struct** (`FcmResult`): fixed return payload, not a sum type.
//! - **Enum** (`FcmError`): closed failure modes, `thiserror`-derived so
//!   callers can `match` on them or display with the standard `Error` trait.
//!
//! ## Out-parameter trait methods
//!
//! `FcmBackend::compute_distances` and `update_centroids` take a `&mut
//! Array2<f32>` that the caller preallocates once per FCM run. This keeps
//! the hot loop free of per-iteration heap allocations on both paths (the
//! CPU backend reuses its internal BLAS scratch; the GPU backend writes
//! the D2H'd results into the caller's buffer).

use ndarray::{Array2, ArrayView2, Zip};
use tracing::{info, warn};

pub mod cpu;
pub mod cuda;

// ============================================================================
// Types shared by both backends
// ============================================================================

/// Type alias for an optional shutdown-check callback used by long-running FCM.
pub type CancelFn<'a> = Option<&'a (dyn Fn() -> bool + Sync)>;

/// Result of a Fuzzy C-Means run.
#[derive(Debug)]
pub struct FcmResult {
    /// Membership matrix (n × K), rows sum to 1.0.
    pub membership: Array2<f32>,
    /// Centroid matrix (K × d).
    pub centroids: Array2<f32>,
    /// Number of iterations run (1-indexed — 0 means zero iterations).
    pub iterations: usize,
    /// Whether convergence was reached (max_change < tolerance).
    pub converged: bool,
    /// Whether the run was cancelled via `should_cancel`.
    pub cancelled: bool,
    /// Sum of weighted squared distances Σᵢⱼ uᵢⱼᵐ · D²ᵢⱼ. f64 accumulator
    /// for numerical stability; final matrices stay f32.
    pub inertia: f64,
}

/// CUDA-precision selector for the CUDA backend. Mirrors the legacy
/// `cron.gpu_fcm_precision` string: "fp32" → `Fp32`, "fp16" → `Fp16`,
/// "bf16" → `Bf16`; anything else → `Fp32` (the widest-compat choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPrecision {
    /// Baseline: cuBLAS SGEMM with f32 data. Works on any CUDA GPU.
    Fp32,
    /// Mixed precision: f16 inputs, fp32 accumulator, Tensor Cores on CC ≥ 7.0.
    Fp16,
    /// bf16 inputs, fp32 accumulator. Same throughput as fp16 on CC ≥ 8.0
    /// but with fp32's exponent range — robust on un-normalized data.
    Bf16,
}

impl GpuPrecision {
    /// Parse the `cron.gpu_fcm_precision` config string.
    pub fn parse(s: &str) -> Self {
        match s {
            "fp16" | "FP16" | "f16" => Self::Fp16,
            "bf16" | "BF16" | "bfloat16" => Self::Bf16,
            _ => Self::Fp32,
        }
    }

    /// Auto-upgrade fp16 → bf16 if a data-value magnitude exceeds ~1000
    /// (fp16 saturates at ±65504; normalized embeddings never trigger this,
    /// but a hand-rolled Vec<f32> with un-L2-normalized values would).
    pub fn auto_adjust_for_un_normalized(self, max_abs_value: f32) -> Self {
        match self {
            Self::Fp16 if max_abs_value > 1000.0 => {
                warn!(
                    max_abs = max_abs_value,
                    "Un-normalized embeddings detected (|v| > 1000); auto-switching fp16 → bf16"
                );
                Self::Bf16
            }
            other => other,
        }
    }
}

/// Construction-time backend selector.
#[derive(Debug, Clone, Copy)]
pub enum BackendChoice {
    /// Try CUDA with the given precision. On construction failure,
    /// `make_backend` falls back to CPU and logs a warning.
    Cuda(GpuPrecision),
    /// Force the CPU backend. Used by tests and by the dispatcher's
    /// fallback path.
    Cpu,
}

/// Closed set of failure modes surfaced by the FCM backends.
#[derive(Debug, thiserror::Error)]
pub enum FcmError {
    /// CUDA backend construction failed (no device, driver error, OOM, …).
    #[error("CUDA init failed: {0}")]
    CudaInit(String),
    /// A CUDA kernel or cuBLAS call failed mid-iteration.
    #[error("CUDA kernel launch failed: {0}")]
    CudaLaunch(String),
    /// The CPU backend encountered a runtime error (OOM, BLAS error).
    /// Kept as a distinct variant from `Config` so callers that want to
    /// distinguish "couldn't start" from "something went wrong mid-compute"
    /// can do so. Not currently produced — reserved for future BLAS-error
    /// plumbing.
    #[allow(dead_code)]
    #[error("CPU backend error: {0}")]
    Cpu(String),
    /// Invalid FCM parameters (k = 0, m ≤ 1, shape mismatch, …).
    #[error("invalid FCM parameters: {0}")]
    Config(String),
}

// ============================================================================
// FcmBackend trait
// ============================================================================

/// Compute surface abstracted across the CUDA and CPU backends.
///
/// Implementations own the input data matrix (uploaded once at construction)
/// and any internal scratch buffers needed for the two GEMMs. The trait's
/// two methods fill caller-provided output buffers in place — no per-iteration
/// heap allocation.
pub trait FcmBackend: Send {
    /// Number of rows (data points).
    fn n(&self) -> usize;

    /// Dimensionality of each data point.
    fn d(&self) -> usize;

    /// Human-readable label for logging and telemetry.
    fn name(&self) -> &'static str;

    /// Compute squared distances `D²ᵢⱼ = max(‖xᵢ‖² + ‖cⱼ‖² − 2·Sᵢⱼ, ε)`
    /// where `Sᵢⱼ = Σₗ xᵢₗ · cⱼₗ`. Writes into `dist_sq_out` (n × K).
    fn compute_distances(
        &mut self,
        centroids: &Array2<f32>,
        dist_sq_out: &mut Array2<f32>,
    ) -> Result<(), FcmError>;

    /// Compute new centroids `Cⱼ = Σᵢ uᵢⱼᵐ · xᵢ / Σᵢ uᵢⱼᵐ`. Writes into
    /// `centroids_out` (K × d).
    fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f32>,
        centroids_out: &mut Array2<f32>,
    ) -> Result<(), FcmError>;
}

// ============================================================================
// Backend factory
// ============================================================================

/// Construct a backend honoring `choice`. When `choice` is `Cuda(...)` and
/// CUDA init fails, logs a warning and returns a CPU backend — this is the
/// runtime CPU-fallback hook.
pub fn make_backend(
    data: Array2<f32>,
    k: usize,
    choice: BackendChoice,
) -> Result<Box<dyn FcmBackend>, FcmError> {
    match choice {
        BackendChoice::Cuda(precision) => {
            let max_abs = data.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
            let adjusted = precision.auto_adjust_for_un_normalized(max_abs);
            match cuda::CudaFcmBackend::new(&data, k, adjusted) {
                Ok(b) => {
                    info!(backend = b.name(), n = data.nrows(), k, "FCM backend: CUDA");
                    Ok(Box::new(b))
                }
                Err(e) => {
                    warn!(error = %e, "CUDA backend init failed; falling back to CPU");
                    Ok(Box::new(cpu::CpuFcmBackend::new(data, k)?))
                }
            }
        }
        BackendChoice::Cpu => {
            info!(n = data.nrows(), k, "FCM backend: CPU");
            Ok(Box::new(cpu::CpuFcmBackend::new(data, k)?))
        }
    }
}

// ============================================================================
// k-means++ initialization
// ============================================================================

/// Select K initial centroids from data using k-means++ seeding.
pub fn kmeans_plus_plus_init(data: ArrayView2<f32>, k: usize) -> Array2<f32> {
    use rand::Rng;

    let n = data.nrows();
    let d = data.ncols();
    let mut rng = rand::rng();
    let mut centroids = Array2::<f32>::zeros((k, d));

    let first = rng.random_range(0..n);
    centroids.row_mut(0).assign(&data.row(first));

    let data_norms: Vec<f32> = (0..n).map(|i| data.row(i).dot(&data.row(i))).collect();
    let mut min_dist_sq = vec![f32::MAX; n];

    for c_idx in 1..k {
        let prev = centroids.row(c_idx - 1);
        let prev_norm = prev.dot(&prev);
        for i in 0..n {
            let dot = data.row(i).dot(&prev);
            let d2 = (data_norms[i] + prev_norm - 2.0 * dot).max(0.0);
            if d2 < min_dist_sq[i] {
                min_dist_sq[i] = d2;
            }
        }

        let total: f32 = min_dist_sq.iter().sum();
        if total <= 0.0 {
            let idx = rng.random_range(0..n);
            centroids.row_mut(c_idx).assign(&data.row(idx));
            continue;
        }

        let threshold = rng.random_range(0.0..total);
        let mut cumulative = 0.0_f32;
        let mut chosen = n - 1;
        for (i, &v) in min_dist_sq.iter().enumerate().take(n) {
            cumulative += v;
            if cumulative >= threshold {
                chosen = i;
                break;
            }
        }
        centroids.row_mut(c_idx).assign(&data.row(chosen));
    }

    centroids
}

// ============================================================================
// FCM iteration loop — backend-parametric
// ============================================================================

/// Run Fuzzy C-Means to convergence (or `max_iters`) on the given backend.
///
/// This function owns the iteration loop and the membership ping-pong.
/// The backend handles only the two GEMM-heavy steps (compute_distances,
/// update_centroids). Membership update happens elementwise on the CPU.
///
/// `data_for_init` is used **only** by `kmeans_plus_plus_init` when
/// `initial_centroids` is `None` — the actual compute data is owned by
/// the backend and uploaded at construction time.
#[allow(clippy::too_many_arguments)]
pub fn run(
    backend: &mut dyn FcmBackend,
    data_for_init: ArrayView2<'_, f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    should_cancel: CancelFn<'_>,
    initial_centroids: Option<Array2<f32>>,
) -> Result<FcmResult, FcmError> {
    let n = backend.n();
    let d = backend.d();

    if k == 0 || k > n {
        return Err(FcmError::Config(format!(
            "k must be in [1, n]; got k={k}, n={n}"
        )));
    }
    if m <= 1.0 {
        return Err(FcmError::Config(format!(
            "fuzziness m must be > 1.0; got {m}"
        )));
    }

    let m_f32 = m as f32;
    let exponent = (2.0 / (m - 1.0)) as f32;
    let eps_dist: f32 = 1e-8;

    let mut centroids = match initial_centroids {
        Some(c) if c.nrows() == k && c.ncols() == d => {
            info!(
                k,
                d,
                backend = backend.name(),
                "FCM warm-start (LMDB centroids)"
            );
            c
        }
        _ => {
            info!(k, d, backend = backend.name(), "FCM cold-start (k-means++)");
            kmeans_plus_plus_init(data_for_init, k)
        }
    };

    // Membership ping-pong + reused per-iter buffers.
    let mut membership_a = Array2::<f32>::zeros((n, k));
    let mut membership_b = Array2::<f32>::zeros((n, k));
    let mut dist_sq = Array2::<f32>::zeros((n, k));
    let mut u_pow_m = Array2::<f32>::zeros((n, k));
    let mut new_centroids = Array2::<f32>::zeros((k, d));

    let mut cur: usize = 0;
    let mut iterations = 0;
    let mut converged = false;
    let mut cancelled = false;

    for iter in 0..max_iters {
        iterations = iter + 1;

        if let Some(cancel) = should_cancel
            && cancel()
        {
            cancelled = true;
            break;
        }

        // Step 1: distance matrix.
        backend.compute_distances(&centroids, &mut dist_sq)?;

        // Step 2: membership update into the non-cur buffer, tracking
        // max_change in the same pass (no clone, no separate subtract).
        let max_change = if cur == 0 {
            update_membership(
                &dist_sq,
                &membership_a,
                &mut membership_b,
                m,
                k,
                n,
                exponent,
                eps_dist,
            )
        } else {
            update_membership(
                &dist_sq,
                &membership_b,
                &mut membership_a,
                m,
                k,
                n,
                exponent,
                eps_dist,
            )
        };

        cur ^= 1;

        if max_change < tolerance {
            converged = true;
        }

        // Step 3: u_pow_m ← cur_mem.powf(m).
        {
            let cur_mem = current_buffer(&membership_a, &membership_b, cur);
            Zip::from(&mut u_pow_m)
                .and(cur_mem)
                .for_each(|dst, &src| *dst = src.powf(m_f32));
        }

        // Step 4: new centroids.
        backend.update_centroids(&u_pow_m, &mut new_centroids)?;
        std::mem::swap(&mut centroids, &mut new_centroids);

        if converged {
            info!(
                iterations,
                max_change = format!("{:.2e}", max_change),
                backend = backend.name(),
                "FCM converged"
            );
            break;
        }
    }

    if !converged && !cancelled {
        warn!(
            iterations,
            max_iters,
            backend = backend.name(),
            "FCM did not converge within max_iters"
        );
    }

    // Extract final membership.
    let mut membership_out = Array2::<f32>::zeros((n, k));
    {
        let cur_mem = current_buffer(&membership_a, &membership_b, cur);
        membership_out.assign(cur_mem);
    }

    // Compute inertia = Σᵢⱼ uᵢⱼᵐ · D²ᵢⱼ using one more distance pass
    // against the final centroids for consistency.
    let _ = backend.compute_distances(&centroids, &mut dist_sq);
    Zip::from(&mut u_pow_m)
        .and(&membership_out)
        .for_each(|dst, &src| *dst = src.powf(m_f32));
    let mut inertia: f64 = 0.0;
    for i in 0..n {
        for j in 0..k {
            inertia += (u_pow_m[[i, j]] as f64) * (dist_sq[[i, j]] as f64);
        }
    }

    Ok(FcmResult {
        membership: membership_out,
        centroids,
        iterations,
        converged,
        cancelled,
        inertia,
    })
}

// ============================================================================
// Internal helpers — shared between backends
// ============================================================================

#[inline]
pub(crate) fn current_buffer<'a>(
    a: &'a Array2<f32>,
    b: &'a Array2<f32>,
    cur: usize,
) -> &'a Array2<f32> {
    if cur == 0 { a } else { b }
}

/// Update the membership matrix in-place: compute new μᵢⱼ into `next`,
/// tracking the element-wise max |new − old| against `prev` in the same
/// pass (no clone, no separate subtract). Returns max_change as f64.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn update_membership(
    dist_sq: &Array2<f32>,
    prev: &Array2<f32>,
    next: &mut Array2<f32>,
    m: f64,
    k: usize,
    n: usize,
    exponent: f32,
    eps_dist: f32,
) -> f64 {
    let mut max_change: f64 = 0.0;

    if (m - 2.0).abs() < 1e-12 {
        // Optimized path for m=2: μᵢⱼ = (1/D²ᵢⱼ) / Σₗ (1/D²ᵢₗ).
        for i in 0..n {
            let mut inv_sum: f32 = 0.0;
            for j in 0..k {
                inv_sum += 1.0 / dist_sq[[i, j]].max(eps_dist);
            }
            let inv_sum = if inv_sum > 0.0 { inv_sum } else { 1.0 };
            for j in 0..k {
                let new_mu = (1.0 / dist_sq[[i, j]].max(eps_dist)) / inv_sum;
                let old_mu = prev[[i, j]];
                let delta = (new_mu - old_mu).abs() as f64;
                if delta > max_change {
                    max_change = delta;
                }
                next[[i, j]] = new_mu;
            }
        }
    } else {
        // General case: μᵢⱼ = 1 / Σₗ (Dᵢⱼ / Dᵢₗ)^(2/(m-1)).
        for i in 0..n {
            for j in 0..k {
                let dij = dist_sq[[i, j]].max(eps_dist);
                let mut denom_sum: f32 = 0.0;
                for l in 0..k {
                    let dil = dist_sq[[i, l]].max(eps_dist);
                    denom_sum += (dij / dil).powf(exponent);
                }
                let new_mu = 1.0 / denom_sum.max(eps_dist);
                let old_mu = prev[[i, j]];
                let delta = (new_mu - old_mu).abs() as f64;
                if delta > max_change {
                    max_change = delta;
                }
                next[[i, j]] = new_mu;
            }
        }
    }

    max_change
}
