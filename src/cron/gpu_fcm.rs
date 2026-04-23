//! cuBLAS-accelerated FCM distance and centroid computation.
//!
//! Primary compute backend when the `cuda` feature is enabled. Accelerates
//! the two BLAS-bound operations in each FCM iteration via cuBLAS GEMM:
//!
//! 1. Distance matrix: D²ᵢₖ = ||xᵢ||² + ||cₖ||² − 2·xᵢ·cₖ   ← GEMM
//! 2. Centroid update: Cₖ = (Uᵐ)ᵀ·X / colsum(Uᵐ)             ← GEMM
//!
//! Membership update stays on CPU (elementwise, negligible cost).
//!
//! Two precision paths (Phase 5 of OOM fix):
//! - fp32 SGEMM — `GpuFcm::new` — original path, compatible with any CUDA GPU.
//! - fp16 Tensor Cores + fp32 accumulate — `GpuFcm::new_fp16` — ~2× GEMM
//!   throughput and ~half the VRAM on Ada Lovelace (CC 8.9) / Hopper (9.0).
//!   Requires GPUs with Tensor Cores that support the fp16 path (CC >= 7.0).
//!   The fp16 path keeps accumulators in fp32 (`CUBLAS_COMPUTE_32F`) — critical
//!   for FCM convergence; naive fp16 accumulation loses too much precision
//!   in the distance math.

use cudarc::cublas::{CudaBlas, Gemm, GemmConfig};
use cudarc::driver::{CudaContext, CudaSlice, CudaStream};
use half::{bf16, f16};
use ndarray::Array2;
use std::sync::Arc;

/// GPU FCM precision selector. Maps directly from `cron.gpu_fcm_precision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPrecision {
    /// Baseline: cuBLAS SGEMM with f32 data. Supported on every CUDA GPU.
    Fp32,
    /// Mixed precision: f16 data on device, fp32 accumulator, Tensor Cores
    /// enabled. Requires CC ≥ 7.0 (Volta+). 2× GEMM throughput and half VRAM
    /// vs Fp32 on Ada Lovelace.
    Fp16,
    /// Alternative mixed precision: bf16 data on device, fp32 accumulator.
    /// bf16 shares fp32's exponent range (8 bits) — safer than fp16 for
    /// un-normalized vectors where magnitudes can exceed ±65504. Slightly
    /// lower mantissa precision (7 bits vs 10 bits) but rarely matters for
    /// L2-normalized embeddings. Ada Lovelace / Hopper support bf16 Tensor
    /// Cores with the same ~2× throughput as fp16.
    Bf16,
}

impl GpuPrecision {
    pub fn parse(s: &str) -> Self {
        match s {
            "fp16" | "FP16" | "f16" => Self::Fp16,
            "bf16" | "BF16" | "bfloat16" => Self::Bf16,
            _ => Self::Fp32,
        }
    }

    /// Phase 11 auto-detect: if the first batch of embeddings has any value
    /// with magnitude > 1000 (a cheap indicator of un-normalized data), prefer
    /// Bf16 over Fp16. Returns `self` unchanged otherwise.
    pub fn auto_adjust_for_un_normalized(self, max_abs_value: f32) -> Self {
        match self {
            Self::Fp16 if max_abs_value > 1000.0 => {
                tracing::warn!(
                    max_abs = max_abs_value,
                    "Un-normalized embeddings detected (|v| > 1000); auto-switching GPU FCM from fp16 to bf16"
                );
                Self::Bf16
            }
            other => other,
        }
    }
}

// Note: cuBLASLt epilogue fusion was evaluated and rejected. cuBLASLt's
// RELU_BIAS epilogue computes `max(α·A·B + bias, 0)` where `bias` is a
// per-output-row vector, but FCM's distance reduction needs per-(i, j)
// addition (`‖xᵢ‖² + ‖cⱼ‖² − 2·Sᵢⱼ`) — the two norm vectors live on
// orthogonal axes and cannot both be fused through a single-axis bias.
// The fused post-GEMM reduction instead lives in `src/fcm/cuda/kernels.cu`
// (Option D-min): a custom CUDA kernel reads the fp16/bf16 dot-product
// buffer + fp32 norm vectors on-device and writes fp32 D² directly, with
// no host D2H of the fp16/bf16 dot buffer or host-side f64 reduction.

/// GPU-accelerated FCM helper. Holds device-resident data and reusable buffers.
pub struct GpuFcm {
    _ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    blas: CudaBlas,
    /// Data matrix X on device (n × d), uploaded once, row-major.
    dev_x: CudaSlice<f32>,
    /// Precomputed ||xᵢ||² on device (n × 1).
    dev_x_norms: CudaSlice<f32>,
    /// Centroid matrix on device (K × d), updated each iteration.
    dev_centroids: CudaSlice<f32>,
    /// Dot-product result S = X · Cᵀ on device (n × K).
    dev_dot: CudaSlice<f32>,
    /// Uᵐ matrix on device (n × K) for centroid update.
    dev_u_pow_m: CudaSlice<f32>,
    /// Result of (Uᵐ)ᵀ · X on device (K × d).
    dev_numerator: CudaSlice<f32>,
    n: usize,
    d: usize,
    k: usize,
}

impl GpuFcm {
    /// Create a new GpuFcm, uploading data to the GPU.
    ///
    /// `data` is an f64 (n × d) ndarray — we convert to f32 for GPU.
    pub fn new(data: &Array2<f64>, k: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let n = data.nrows();
        let d = data.ncols();

        let ctx = CudaContext::new(0)?;
        let stream = ctx.default_stream();
        let blas = CudaBlas::new(stream.clone())?;

        // Convert f64 → f32 in row-major layout
        let x_f32: Vec<f32> = data.iter().map(|&v| v as f32).collect();
        let dev_x = stream.clone_htod(&x_f32)?;

        // Precompute ||xᵢ||²
        let x_norms: Vec<f32> = (0..n)
            .map(|i| {
                let start = i * d;
                let end = start + d;
                data.as_slice().expect("data should be contiguous")[start..end]
                    .iter()
                    .map(|&v| (v * v) as f32)
                    .sum::<f32>()
            })
            .collect();
        let dev_x_norms = stream.clone_htod(&x_norms)?;

        // Allocate reusable buffers
        let dev_centroids = stream.alloc_zeros::<f32>(k * d)?;
        let dev_dot = stream.alloc_zeros::<f32>(n * k)?;
        let dev_u_pow_m = stream.alloc_zeros::<f32>(n * k)?;
        let dev_numerator = stream.alloc_zeros::<f32>(k * d)?;

        Ok(Self {
            _ctx: ctx,
            stream,
            blas,
            dev_x,
            dev_x_norms,
            dev_centroids,
            dev_dot,
            dev_u_pow_m,
            dev_numerator,
            n,
            d,
            k,
        })
    }

    /// Compute squared distance matrix D²[i,k] using cuBLAS SGEMM.
    ///
    /// Returns host-side (n × K) f64 array.
    pub fn compute_distances(
        &mut self,
        centroids: &Array2<f64>,
    ) -> Result<Array2<f64>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        // Upload centroids to GPU (K × d, row-major)
        let c_f32: Vec<f32> = centroids.iter().map(|&v| v as f32).collect();
        self.stream.memcpy_htod(&c_f32, &mut self.dev_centroids)?;

        // Compute S = X · Cᵀ (n × K), stored row-major = col-major Sᵀ (K × n).
        //
        // Convention: cuBLAS is column-major; our buffers are row-major.
        // A row-major matrix (r, c) has the same bytes as a column-major (c, r).
        // Specifically:
        //   dev_centroids row-major (K, d) → col-major (d, K), lda = d
        //   dev_x         row-major (n, d) → col-major (d, n), ldb = d
        //   dev_dot       row-major (n, K) → col-major (K, n), ldc = K
        //
        // For GEMM output col-major (K, n) = Sᵀ we compute
        //   Sᵀ = Cᵀ_row · X_row^T   (mathematically equivalent to writing
        //   S[i, j] = Σ_l X[i, l] · C[j, l])
        // Which in col-major with op(A)·op(B) = (m × k) · (k × n) is
        //   op(A) = (C_col)ᵀ = (K × d)  ← transa = OP_T
        //   op(B) = X_col    = (d × n)  ← transb = OP_N
        //
        // Earlier versions of this code used transa = OP_N (reading A as
        // (d × K) col-major with stride d), which told cuBLAS to interpret
        // the first K × d bytes as a (m = K, k_inner = d) matrix at col
        // stride d. With d ≫ K, that reads past the buffer.
        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    m: k as i32,
                    n: n as i32,
                    k: d as i32,
                    alpha: 1.0f32,
                    lda: d as i32,
                    ldb: d as i32,
                    beta: 0.0f32,
                    ldc: k as i32,
                },
                &self.dev_centroids,
                &self.dev_x,
                &mut self.dev_dot,
            )?;
        }

        // Download S to host
        let dot_host = self.stream.clone_dtoh(&self.dev_dot)?;
        // Download x_norms
        let x_norms_host = self.stream.clone_dtoh(&self.dev_x_norms)?;

        // Compute c_norms on CPU (small: K values)
        let c_norms: Vec<f32> = (0..k)
            .map(|j| {
                (0..d)
                    .map(|dim| {
                        let v = centroids[[j, dim]] as f32;
                        v * v
                    })
                    .sum::<f32>()
            })
            .collect();

        // Build D²[i,k] = ||x_i||² + ||c_k||² - 2·S[i,k]
        let mut dist_sq = Array2::<f64>::zeros((n, k));
        for i in 0..n {
            for j in 0..k {
                let s = dot_host[i * k + j] as f64;
                let d2 = (x_norms_host[i] as f64 + c_norms[j] as f64 - 2.0 * s).max(1e-16);
                dist_sq[[i, j]] = d2;
            }
        }

        Ok(dist_sq)
    }

    /// Compute new centroids using cuBLAS: C_new = (Uᵐ)ᵀ · X / colsum(Uᵐ)
    ///
    /// Returns host-side (K × d) f64 array.
    pub fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f64>,
    ) -> Result<Array2<f64>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        // Upload Uᵐ to GPU (n × K, row-major)
        let u_f32: Vec<f32> = u_pow_m.iter().map(|&v| v as f32).collect();
        self.stream.memcpy_htod(&u_f32, &mut self.dev_u_pow_m)?;

        // Compute numerator = (Uᵐ)ᵀ · X (K × d)
        // Row-major Uᵐ (n×k) = col-major (Uᵐ)ᵀ (k×n)
        // Row-major X (n×d) = col-major Xᵀ (d×n)
        // We want R = (Uᵐ)ᵀ · X (k×d), stored row-major = col-major Rᵀ (d×k)
        // Rᵀ = Xᵀ · (Uᵐ) (d×k) but we need (Uᵐ)ᵀ · X
        // In col-major: numeratorᵀ (d×k) = Xᵀ (d×n) · Uᵐ as col-major = X_rowmajor · U_colmajor_view
        // Hmm, let me think again...
        // Col-major perspective:
        //   U_row_major (n×k) stored flat = col-major view as (k×n) matrix (transposed)
        //   X_row_major (n×d) stored flat = col-major view as (d×n) matrix (transposed)
        //   We want result = Uᵀ · X (k×d) in row-major = col-major (d×k)
        //   = X_col_major_view (d×n) · U_col_major_view_transposed (n×k)
        //   So gemm(NoTrans, Trans, d, k, n, X, U)
        //   But U stored as col-major (k×n), Trans gives (n×k) — that's what we want

        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    m: d as i32,
                    n: k as i32,
                    k: n as i32,
                    alpha: 1.0f32,
                    lda: d as i32,
                    ldb: k as i32,
                    beta: 0.0f32,
                    ldc: d as i32,
                },
                &self.dev_x,
                &self.dev_u_pow_m,
                &mut self.dev_numerator,
            )?;
        }

        // Download numerator
        let num_host = self.stream.clone_dtoh(&self.dev_numerator)?;

        // Column sums of Uᵐ on CPU
        let col_sums: Vec<f64> = (0..k)
            .map(|j| (0..n).map(|i| u_pow_m[[i, j]]).sum::<f64>())
            .collect();

        // Build centroid matrix, dividing by column sums
        let mut centroids = Array2::<f64>::zeros((k, d));
        for j in 0..k {
            let denom = col_sums[j].max(1e-16);
            for dim in 0..d {
                centroids[[j, dim]] = num_host[j * d + dim] as f64 / denom;
            }
        }

        Ok(centroids)
    }
}

// ============================================================================
// Phase 5: mixed-precision fp16 FCM (Tensor Cores)
// ============================================================================

/// Mixed-precision GPU FCM helper — fp16 storage on device, fp32 accumulator
/// inside cuBLAS (`CUBLAS_COMPUTE_32F`), Tensor Cores enabled.
///
/// Mirrors `GpuFcm` field-for-field but with f16 device buffers. Host inputs
/// stay f32 (cheap: `half::f16::from_f32` is a single FMA-reciprocal).
///
/// VRAM footprint for n=113k, K=100, d=384 in fp16:
///   dev_x         = n·d·2 =  86 MB   (was 172 MB in fp32)
///   dev_x_norms   = n·4   = 450 KB   (f32 for accumulator stability)
///   dev_centroids = K·d·2 = 75 KB
///   dev_dot       = n·K·2 = 22 MB    (was 44 MB in fp32)
///   dev_u_pow_m   = n·K·2 = 22 MB
///   dev_numerator = K·d·2 = 75 KB
///   Total VRAM    ≈ 130 MB — fits comfortably on RTX 4060 Ti's 8 GiB.
pub struct GpuFcmFp16 {
    _ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    blas: CudaBlas,
    /// Data matrix X on device (n × d), fp16, uploaded once, row-major.
    dev_x: CudaSlice<f16>,
    /// Precomputed ||xᵢ||² on device (n × 1), kept in f32 for stability.
    dev_x_norms: CudaSlice<f32>,
    /// Centroid matrix (K × d), fp16, updated each iteration.
    dev_centroids: CudaSlice<f16>,
    /// Dot-product result S = X · Cᵀ (n × K), fp16.
    dev_dot: CudaSlice<f16>,
    /// Uᵐ matrix (n × K), fp16.
    dev_u_pow_m: CudaSlice<f16>,
    /// (Uᵐ)ᵀ · X result (K × d), fp16.
    dev_numerator: CudaSlice<f16>,
    n: usize,
    d: usize,
    k: usize,
}

impl GpuFcmFp16 {
    /// Create a new fp16 FCM helper, uploading data to the GPU.
    ///
    /// `data` is an f32 (n × d) ndarray — converted to fp16 on the host, then
    /// copied to device. `||xᵢ||²` is precomputed in f32 on the host (uses
    /// f32 squared data; we don't round the norm itself to f16 because that
    /// would lose magnitude information near zero).
    pub fn new(data: &Array2<f32>, k: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let n = data.nrows();
        let d = data.ncols();

        let ctx = CudaContext::new(0)?;
        let stream = ctx.default_stream();
        let blas = CudaBlas::new(stream.clone())?;

        // Host-side f32 → f16 conversion (single pass, row-major).
        let x_f16: Vec<f16> = data.iter().map(|&v| f16::from_f32(v)).collect();
        let dev_x = stream.clone_htod(&x_f16)?;

        // Precompute ||xᵢ||² in f32 (accumulator precision matters).
        let x_norms: Vec<f32> = (0..n)
            .map(|i| {
                let start = i * d;
                let end = start + d;
                data.as_slice().expect("data should be contiguous")[start..end]
                    .iter()
                    .map(|&v| v * v)
                    .sum::<f32>()
            })
            .collect();
        let dev_x_norms = stream.clone_htod(&x_norms)?;

        let dev_centroids = stream.alloc_zeros::<f16>(k * d)?;
        let dev_dot = stream.alloc_zeros::<f16>(n * k)?;
        let dev_u_pow_m = stream.alloc_zeros::<f16>(n * k)?;
        let dev_numerator = stream.alloc_zeros::<f16>(k * d)?;

        Ok(Self {
            _ctx: ctx,
            stream,
            blas,
            dev_x,
            dev_x_norms,
            dev_centroids,
            dev_dot,
            dev_u_pow_m,
            dev_numerator,
            n,
            d,
            k,
        })
    }

    /// Compute squared distance matrix D²[i,k] on device using fp16 Tensor-Core
    /// GEMM. The post-GEMM `||x||² + ||c||² - 2·S` reduction is done on host
    /// in f64 for precision.
    ///
    /// On GPUs without Tensor Cores (CC < 7.0), cuBLAS transparently falls
    /// back to SIMT fp16 arithmetic (no throughput win).
    pub fn compute_distances(
        &mut self,
        centroids: &Array2<f32>,
    ) -> Result<Array2<f64>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        // Upload centroids (f32 → f16 on host, row-major).
        let c_f16: Vec<f16> = centroids.iter().map(|&v| f16::from_f32(v)).collect();
        self.stream.memcpy_htod(&c_f16, &mut self.dev_centroids)?;

        // S = X · Cᵀ (n × K) via fp16 cuBLAS GEMM.
        // Same col-major convention as the fp32 path: transa = OP_T so the
        // row-major (K, d) centroid buffer is interpreted as (K, d) after
        // the implicit transpose. See GpuFcm::compute_distances for a full
        // derivation.
        //
        // NOTE: `alpha` / `beta` are `half::f16`, but cudarc's `Gemm<f16>` impl
        // up-casts them to `f32` via `.to_f32()` and invokes `cublasGemmEx`
        // with `CUBLAS_COMPUTE_32F` — so the GEMM accumulator is fp32, which
        // is what FCM convergence requires. See
        // cudarc-0.19.4/src/cublas/safe/gemm.rs:71-94.
        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    m: k as i32,
                    n: n as i32,
                    k: d as i32,
                    alpha: f16::from_f32(1.0),
                    lda: d as i32,
                    ldb: d as i32,
                    beta: f16::from_f32(0.0),
                    ldc: k as i32,
                },
                &self.dev_centroids,
                &self.dev_x,
                &mut self.dev_dot,
            )?;
        }

        // Download S (fp16) and x_norms (f32).
        let dot_host_f16 = self.stream.clone_dtoh(&self.dev_dot)?;
        let x_norms_host = self.stream.clone_dtoh(&self.dev_x_norms)?;

        // c_norms in f32 on host (K tiny).
        let c_norms: Vec<f32> = (0..k)
            .map(|j| {
                (0..d)
                    .map(|dim| {
                        let v = centroids[[j, dim]];
                        v * v
                    })
                    .sum::<f32>()
            })
            .collect();

        // Build D² in f64 for numerical stability. Floor at 1e-6 (higher than
        // the fp32 path's 1e-16 because fp16's representable minimum is ~6e-8
        // and subtracting two near-equal fp16 values loses precision).
        let mut dist_sq = Array2::<f64>::zeros((n, k));
        for i in 0..n {
            for j in 0..k {
                let s = dot_host_f16[i * k + j].to_f32() as f64;
                let d2 = (x_norms_host[i] as f64 + c_norms[j] as f64 - 2.0 * s).max(1e-6);
                dist_sq[[i, j]] = d2;
            }
        }

        Ok(dist_sq)
    }

    /// Compute new centroids: C_new = (Uᵐ)ᵀ · X / colsum(Uᵐ) via fp16 GEMM.
    pub fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f32>,
    ) -> Result<Array2<f32>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        let u_f16: Vec<f16> = u_pow_m.iter().map(|&v| f16::from_f32(v)).collect();
        self.stream.memcpy_htod(&u_f16, &mut self.dev_u_pow_m)?;

        // NOTE: `alpha` / `beta` are `half::f16`, but cudarc's `Gemm<f16>` impl
        // up-casts them to `f32` via `.to_f32()` and invokes `cublasGemmEx`
        // with `CUBLAS_COMPUTE_32F` — accumulator is fp32.
        // See cudarc-0.19.4/src/cublas/safe/gemm.rs:71-94.
        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    m: d as i32,
                    n: k as i32,
                    k: n as i32,
                    alpha: f16::from_f32(1.0),
                    lda: d as i32,
                    ldb: k as i32,
                    beta: f16::from_f32(0.0),
                    ldc: d as i32,
                },
                &self.dev_x,
                &self.dev_u_pow_m,
                &mut self.dev_numerator,
            )?;
        }

        let num_host_f16 = self.stream.clone_dtoh(&self.dev_numerator)?;

        // Column sums of Uᵐ on CPU in f32 (stable accumulator).
        let col_sums: Vec<f32> = (0..k)
            .map(|j| (0..n).map(|i| u_pow_m[[i, j]]).sum::<f32>())
            .collect();

        let mut centroids = Array2::<f32>::zeros((k, d));
        for j in 0..k {
            let denom = col_sums[j].max(1e-6);
            for dim in 0..d {
                centroids[[j, dim]] = num_host_f16[j * d + dim].to_f32() / denom;
            }
        }

        Ok(centroids)
    }
}

// ============================================================================
// Phase 11: bf16 path for un-normalized vectors
// ============================================================================

/// Mixed-precision GPU FCM helper — **bf16** storage on device with fp32
/// accumulator. Mirrors `GpuFcmFp16` field-for-field but uses `half::bf16`.
///
/// bf16 shares fp32's 8-bit exponent range, so it's safe for un-normalized
/// embeddings where fp16 would saturate at ±65504. Mantissa precision is
/// lower (7 bits vs fp16's 10 bits), but for L2-normalized vectors this
/// difference is below the FCM convergence tolerance.
///
/// Tensor-Core support: CC 8.0+ (Ampere) for bf16 GEMM. Ada Lovelace (8.9)
/// on the user's RTX 4060 Ti supports this natively.
pub struct GpuFcmBf16 {
    _ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    blas: CudaBlas,
    dev_x: CudaSlice<bf16>,
    dev_x_norms: CudaSlice<f32>,
    dev_centroids: CudaSlice<bf16>,
    dev_dot: CudaSlice<bf16>,
    dev_u_pow_m: CudaSlice<bf16>,
    dev_numerator: CudaSlice<bf16>,
    n: usize,
    d: usize,
    k: usize,
}

impl GpuFcmBf16 {
    pub fn new(data: &Array2<f32>, k: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let n = data.nrows();
        let d = data.ncols();

        let ctx = CudaContext::new(0)?;
        let stream = ctx.default_stream();
        let blas = CudaBlas::new(stream.clone())?;

        let x_bf16: Vec<bf16> = data.iter().map(|&v| bf16::from_f32(v)).collect();
        let dev_x = stream.clone_htod(&x_bf16)?;

        let x_norms: Vec<f32> = (0..n)
            .map(|i| {
                let start = i * d;
                let end = start + d;
                data.as_slice().expect("data should be contiguous")[start..end]
                    .iter()
                    .map(|&v| v * v)
                    .sum::<f32>()
            })
            .collect();
        let dev_x_norms = stream.clone_htod(&x_norms)?;

        let dev_centroids = stream.alloc_zeros::<bf16>(k * d)?;
        let dev_dot = stream.alloc_zeros::<bf16>(n * k)?;
        let dev_u_pow_m = stream.alloc_zeros::<bf16>(n * k)?;
        let dev_numerator = stream.alloc_zeros::<bf16>(k * d)?;

        Ok(Self {
            _ctx: ctx,
            stream,
            blas,
            dev_x,
            dev_x_norms,
            dev_centroids,
            dev_dot,
            dev_u_pow_m,
            dev_numerator,
            n,
            d,
            k,
        })
    }

    pub fn compute_distances(
        &mut self,
        centroids: &Array2<f32>,
    ) -> Result<Array2<f64>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        let c_bf16: Vec<bf16> = centroids.iter().map(|&v| bf16::from_f32(v)).collect();
        self.stream.memcpy_htod(&c_bf16, &mut self.dev_centroids)?;

        // transa = OP_T so the row-major (K, d) centroid buffer is
        // interpreted correctly for the (K, d) × (d, n) product. See
        // GpuFcm::compute_distances for the full derivation.
        //
        // NOTE: `alpha` / `beta` are `half::bf16`, but cudarc's `Gemm<bf16>` impl
        // up-casts them to `f32` via `.to_f32()` and invokes `cublasGemmEx` with
        // `CUBLAS_COMPUTE_32F` — accumulator is fp32, safe for FCM convergence
        // even with bf16's 7-bit mantissa.
        // See cudarc-0.19.4/src/cublas/safe/gemm.rs:155-179.
        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    m: k as i32,
                    n: n as i32,
                    k: d as i32,
                    alpha: bf16::from_f32(1.0),
                    lda: d as i32,
                    ldb: d as i32,
                    beta: bf16::from_f32(0.0),
                    ldc: k as i32,
                },
                &self.dev_centroids,
                &self.dev_x,
                &mut self.dev_dot,
            )?;
        }

        let dot_host_bf16 = self.stream.clone_dtoh(&self.dev_dot)?;
        let x_norms_host = self.stream.clone_dtoh(&self.dev_x_norms)?;

        let c_norms: Vec<f32> = (0..k)
            .map(|j| {
                (0..d)
                    .map(|dim| {
                        let v = centroids[[j, dim]];
                        v * v
                    })
                    .sum::<f32>()
            })
            .collect();

        let mut dist_sq = Array2::<f64>::zeros((n, k));
        for i in 0..n {
            for j in 0..k {
                let s = dot_host_bf16[i * k + j].to_f32() as f64;
                let d2 = (x_norms_host[i] as f64 + c_norms[j] as f64 - 2.0 * s).max(1e-6);
                dist_sq[[i, j]] = d2;
            }
        }

        Ok(dist_sq)
    }

    pub fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f32>,
    ) -> Result<Array2<f32>, Box<dyn std::error::Error>> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        let u_bf16: Vec<bf16> = u_pow_m.iter().map(|&v| bf16::from_f32(v)).collect();
        self.stream.memcpy_htod(&u_bf16, &mut self.dev_u_pow_m)?;

        // NOTE: `alpha` / `beta` are `half::bf16`, but cudarc's `Gemm<bf16>` impl
        // up-casts them to `f32` and invokes `cublasGemmEx` with
        // `CUBLAS_COMPUTE_32F` — accumulator is fp32.
        // See cudarc-0.19.4/src/cublas/safe/gemm.rs:155-179.
        unsafe {
            self.blas.gemm(
                GemmConfig {
                    transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                    transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                    m: d as i32,
                    n: k as i32,
                    k: n as i32,
                    alpha: bf16::from_f32(1.0),
                    lda: d as i32,
                    ldb: k as i32,
                    beta: bf16::from_f32(0.0),
                    ldc: d as i32,
                },
                &self.dev_x,
                &self.dev_u_pow_m,
                &mut self.dev_numerator,
            )?;
        }

        let num_host_bf16 = self.stream.clone_dtoh(&self.dev_numerator)?;

        let col_sums: Vec<f32> = (0..k)
            .map(|j| (0..n).map(|i| u_pow_m[[i, j]]).sum::<f32>())
            .collect();

        let mut centroids = Array2::<f32>::zeros((k, d));
        for j in 0..k {
            let denom = col_sums[j].max(1e-6);
            for dim in 0..d {
                centroids[[j, dim]] = num_host_bf16[j * d + dim].to_f32() / denom;
            }
        }

        Ok(centroids)
    }
}
