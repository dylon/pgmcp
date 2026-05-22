//! CUDA FCM backend.
//!
//! Three precision paths dispatched at construction via `GpuPrecision`:
//! - `Fp32` — delegates to the legacy `cron::gpu_fcm::GpuFcm` (cuBLAS SGEMM,
//!   f64 host-side reduction). Kept for tight-tolerance callers that need
//!   IEEE-fp32 semantics.
//! - `Fp16` — cuBLAS `CudaBlas::gemm` with fp16 inputs + fp32 accumulator,
//!   followed by the fused `fused_distance_reduce_fp16` kernel from
//!   `kernels.cu` (no host round-trip of the fp16 dot matrix).
//! - `Bf16` — same as Fp16 but with bf16 storage.
//!
//! The fused fp16 / bf16 paths eliminate the D2H of the fp16/bf16 dot-product
//! buffer plus the host-side f64 reduction loop that the legacy
//! `GpuFcmFp16::compute_distances` / `GpuFcmBf16::compute_distances`
//! performed. On-device c_norms compute eliminates one more host pass.

mod kernels;

use std::sync::Arc;

use cudarc::cublas::{CudaBlas, Gemm, GemmConfig};
use cudarc::driver::{CudaContext, CudaSlice, CudaStream, LaunchConfig, PushKernelArg};
use half::{bf16, f16};
use ndarray::Array2;

use super::{FcmBackend, FcmError, GpuPrecision};
use crate::cron::gpu_fcm::GpuFcm;
use kernels::FcmKernels;

/// Internal dispatch over precisions.
enum CudaInner {
    /// Legacy fp32 path: delegates to cron::gpu_fcm::GpuFcm (f64 host, no fused kernel).
    Fp32(Fp32State),
    /// Fused fp16 path (this module's compute_distances uses the CUDA kernel).
    Fp16(MixedState<f16>),
    /// Fused bf16 path.
    Bf16(MixedState<bf16>),
}

/// Fp32 precision uses the legacy `GpuFcm` helper directly, converting back
/// and forth at the f32/f64 boundary. Numerically identical to the pre-refactor
/// behaviour.
struct Fp32State {
    gpu: GpuFcm,
}

/// Mixed-precision state shared between fp16 and bf16 paths. `T` is the
/// on-device storage type (`half::f16` or `half::bf16`). Accumulators stay
/// fp32 both in the cuBLAS GEMM (via cudarc's `Gemm<T>` impl dispatching to
/// `CUBLAS_COMPUTE_32F`) and in the fused reduction kernel.
struct MixedState<T: DeviceElem> {
    _ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    blas: CudaBlas,
    kernels: FcmKernels,
    dev_x: CudaSlice<T>,
    dev_x_norms: CudaSlice<f32>,
    dev_centroids: CudaSlice<T>,
    dev_dot: CudaSlice<T>,
    dev_c_norms: CudaSlice<f32>,
    dev_dist_sq: CudaSlice<f32>,
    dev_u_pow_m: CudaSlice<T>,
    dev_numerator: CudaSlice<T>,
    /// Phase 10: on-device col-sums buffer used by `update_centroids` to
    /// fold `Σᵢ uᵢⱼᵐ` without a D2H of `dev_u_pow_m`.
    dev_col_sums: CudaSlice<f32>,
    /// Phase 10: on-device fp32 centroids buffer written by the per-(j, dim)
    /// divide kernel. Only this buffer is D2H'd per iteration — the
    /// T-precision `dev_numerator` stays on the device.
    dev_centroids_fp32: CudaSlice<f32>,
    n: usize,
    d: usize,
    k: usize,
}

/// Helper trait for the fp16/bf16 storage element — encapsulates host
/// conversion and kernel selection so the MixedState impl doesn't need a
/// giant `match precision` at every call site.
trait DeviceElem:
    cudarc::driver::DeviceRepr
    + cudarc::driver::ValidAsZeroBits
    + Copy
    + 'static
    + std::fmt::Debug
    + Send
    + Sync
{
    /// Host-side f32 → Self conversion.
    fn from_f32(v: f32) -> Self;

    /// Select the distance-reduction kernel for this storage type.
    fn distance_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction;

    /// Select the c_norms kernel for this storage type.
    fn c_norms_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction;

    /// Select the col-sums reduction kernel (Phase 10).
    fn reduce_col_sums_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction;

    /// Select the numerator-divide kernel (Phase 10).
    fn divide_numerator_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction;
}

impl DeviceElem for f16 {
    fn from_f32(v: f32) -> Self {
        f16::from_f32(v)
    }
    fn distance_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.fused_distance_reduce_fp16
    }
    fn c_norms_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.compute_c_norms_fp16
    }
    fn reduce_col_sums_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.reduce_col_sums_fp16
    }
    fn divide_numerator_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.divide_numerator_by_col_sums_fp16
    }
}

impl DeviceElem for bf16 {
    fn from_f32(v: f32) -> Self {
        bf16::from_f32(v)
    }
    fn distance_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.fused_distance_reduce_bf16
    }
    fn c_norms_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.compute_c_norms_bf16
    }
    fn reduce_col_sums_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.reduce_col_sums_bf16
    }
    fn divide_numerator_kernel(k: &FcmKernels) -> &cudarc::driver::CudaFunction {
        &k.divide_numerator_by_col_sums_bf16
    }
}

impl<T: DeviceElem> MixedState<T>
where
    CudaBlas: Gemm<T>,
{
    fn new(data: &Array2<f32>, k: usize) -> Result<Self, FcmError> {
        let n = data.nrows();
        let d = data.ncols();

        let ctx = CudaContext::new(0).map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let stream = ctx.default_stream();
        let blas = CudaBlas::new(stream.clone()).map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let kernels = FcmKernels::load(&ctx).map_err(|e| FcmError::CudaInit(e.to_string()))?;

        // Upload data (converted to T).
        let x_host: Vec<T> = data.iter().map(|&v| T::from_f32(v)).collect();
        let dev_x = stream
            .clone_htod(&x_host)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;

        // Precompute ‖xᵢ‖² in fp32.
        let x_norms: Vec<f32> = (0..n)
            .map(|i| {
                let start = i * d;
                let end = start + d;
                data.as_slice().expect("data contiguous")[start..end]
                    .iter()
                    .map(|&v| v * v)
                    .sum::<f32>()
            })
            .collect();
        let dev_x_norms = stream
            .clone_htod(&x_norms)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;

        let dev_centroids = stream
            .alloc_zeros::<T>(k * d)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_dot = stream
            .alloc_zeros::<T>(n * k)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_c_norms = stream
            .alloc_zeros::<f32>(k)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_dist_sq = stream
            .alloc_zeros::<f32>(n * k)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_u_pow_m = stream
            .alloc_zeros::<T>(n * k)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_numerator = stream
            .alloc_zeros::<T>(k * d)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        // Phase 10: on-device col_sums + fp32 centroids buffers.
        let dev_col_sums = stream
            .alloc_zeros::<f32>(k)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;
        let dev_centroids_fp32 = stream
            .alloc_zeros::<f32>(k * d)
            .map_err(|e| FcmError::CudaInit(e.to_string()))?;

        Ok(Self {
            _ctx: ctx,
            stream,
            blas,
            kernels,
            dev_x,
            dev_x_norms,
            dev_centroids,
            dev_dot,
            dev_c_norms,
            dev_dist_sq,
            dev_u_pow_m,
            dev_numerator,
            dev_col_sums,
            dev_centroids_fp32,
            n,
            d,
            k,
        })
    }

    /// Upload centroids, run S = X·Cᵀ GEMM, run fused-reduction kernel,
    /// D2H the fp32 dist_sq into `dist_sq_out`.
    fn compute_distances(
        &mut self,
        centroids: &Array2<f32>,
        dist_sq_out: &mut Array2<f32>,
    ) -> Result<(), FcmError> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        // Upload centroids.
        let c_host: Vec<T> = centroids.iter().map(|&v| T::from_f32(v)).collect();
        self.stream
            .memcpy_htod(&c_host, &mut self.dev_centroids)
            .map_err(|e| FcmError::CudaLaunch(format!("centroids htod: {e}")))?;

        // S = X · Cᵀ via cuBLAS mixed-precision GEMM. See
        // cron::gpu_fcm::GpuFcmFp16::compute_distances for the transa=OP_T
        // derivation and the fp32-accumulator invariant.
        //
        // alpha / beta are in T but cudarc's `Gemm<T>` impl up-casts them to
        // f32 via `.to_f32()` and calls `cublasGemmEx(..., CUBLAS_COMPUTE_32F)`.
        unsafe {
            self.blas
                .gemm(
                    GemmConfig {
                        transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                        transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                        m: k as i32,
                        n: n as i32,
                        k: d as i32,
                        alpha: T::from_f32(1.0),
                        lda: d as i32,
                        ldb: d as i32,
                        beta: T::from_f32(0.0),
                        ldc: k as i32,
                    },
                    &self.dev_centroids,
                    &self.dev_x,
                    &mut self.dev_dot,
                )
                .map_err(|e| FcmError::CudaLaunch(format!("GEMM X·Cᵀ: {e}")))?;
        }

        // On-device c_norms[j] = ‖cⱼ‖² — one block per j, 128 threads,
        // block-stride sum in shared memory.
        let k_i32 = k as i32;
        let d_i32 = d as i32;
        let n_i32 = n as i32;
        let eps = 1e-6f32;
        {
            let cfg = LaunchConfig {
                grid_dim: (k as u32, 1, 1),
                block_dim: (128, 1, 1),
                shared_mem_bytes: 0,
            };
            let mut b = self.stream.launch_builder(T::c_norms_kernel(&self.kernels));
            b.arg(&self.dev_centroids);
            b.arg(&mut self.dev_c_norms);
            b.arg(&k_i32);
            b.arg(&d_i32);
            unsafe { b.launch(cfg) }
                .map_err(|e| FcmError::CudaLaunch(format!("c_norms kernel: {e}")))?;
        }

        // Fused distance reduction: dev_dist_sq ← max(x_norms + c_norms − 2·dot, eps).
        {
            let cfg = LaunchConfig {
                grid_dim: (n as u32, 1, 1),
                block_dim: (128, 1, 1),
                shared_mem_bytes: 0,
            };
            let mut b = self
                .stream
                .launch_builder(T::distance_kernel(&self.kernels));
            b.arg(&self.dev_dot);
            b.arg(&self.dev_x_norms);
            b.arg(&self.dev_c_norms);
            b.arg(&mut self.dev_dist_sq);
            b.arg(&n_i32);
            b.arg(&k_i32);
            b.arg(&eps);
            unsafe { b.launch(cfg) }
                .map_err(|e| FcmError::CudaLaunch(format!("fused reduction: {e}")))?;
        }

        // D2H fp32 dist_sq directly into the caller's buffer.
        let flat = self
            .stream
            .clone_dtoh(&self.dev_dist_sq)
            .map_err(|e| FcmError::CudaLaunch(format!("dist_sq dtoh: {e}")))?;
        for i in 0..n {
            for j in 0..k {
                dist_sq_out[[i, j]] = flat[i * k + j];
            }
        }
        Ok(())
    }

    /// Upload Uᵐ, run (Uᵐ)ᵀ·X GEMM, D2H the numerator, divide by col_sums on
    /// host. Col-sums reduction stays on host because the numerator download
    /// Phase 10: on-device fused col-sums + numerator-divide path. The
    /// previous implementation downloaded `dev_numerator` (K·d in
    /// T-precision) plus walked `u_pow_m` host-side for col-sums; now both
    /// stay on the device and only the final fp32 K·d centroids cross the
    /// PCIe boundary per iteration.
    fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f32>,
        centroids_out: &mut Array2<f32>,
    ) -> Result<(), FcmError> {
        let n = self.n;
        let d = self.d;
        let k = self.k;

        let u_host: Vec<T> = u_pow_m.iter().map(|&v| T::from_f32(v)).collect();
        self.stream
            .memcpy_htod(&u_host, &mut self.dev_u_pow_m)
            .map_err(|e| FcmError::CudaLaunch(format!("u_pow_m htod: {e}")))?;

        unsafe {
            self.blas
                .gemm(
                    GemmConfig {
                        transa: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_N,
                        transb: cudarc::cublas::sys::cublasOperation_t::CUBLAS_OP_T,
                        m: d as i32,
                        n: k as i32,
                        k: n as i32,
                        alpha: T::from_f32(1.0),
                        lda: d as i32,
                        ldb: k as i32,
                        beta: T::from_f32(0.0),
                        ldc: d as i32,
                    },
                    &self.dev_x,
                    &self.dev_u_pow_m,
                    &mut self.dev_numerator,
                )
                .map_err(|e| FcmError::CudaLaunch(format!("GEMM (Uᵐ)ᵀ·X: {e}")))?;
        }

        let n_i32 = n as i32;
        let k_i32 = k as i32;
        let d_i32 = d as i32;
        let eps: f32 = 1e-6;

        // Step 1: col_sums[j] = Σᵢ uᵢⱼᵐ on device. One block per j, 128
        // threads, block-stride sum into shared memory, single-thread
        // reduce-write at the end (same shape as compute_c_norms).
        {
            let cfg = LaunchConfig {
                grid_dim: (k as u32, 1, 1),
                block_dim: (128, 1, 1),
                shared_mem_bytes: 0,
            };
            let mut b = self
                .stream
                .launch_builder(T::reduce_col_sums_kernel(&self.kernels));
            b.arg(&self.dev_u_pow_m);
            b.arg(&mut self.dev_col_sums);
            b.arg(&n_i32);
            b.arg(&k_i32);
            unsafe { b.launch(cfg) }
                .map_err(|e| FcmError::CudaLaunch(format!("reduce_col_sums kernel: {e}")))?;
        }

        // Step 2: dev_centroids_fp32[j*d + dim] = numerator[j*d + dim]
        //                                          / max(col_sums[j], eps).
        // One thread per (j, dim) — total K·d threads, 256-thread blocks.
        {
            let total = (k * d) as u32;
            let block = 256u32;
            let grid = total.div_ceil(block);
            let cfg = LaunchConfig {
                grid_dim: (grid, 1, 1),
                block_dim: (block, 1, 1),
                shared_mem_bytes: 0,
            };
            let mut b = self
                .stream
                .launch_builder(T::divide_numerator_kernel(&self.kernels));
            b.arg(&self.dev_numerator);
            b.arg(&self.dev_col_sums);
            b.arg(&mut self.dev_centroids_fp32);
            b.arg(&k_i32);
            b.arg(&d_i32);
            b.arg(&eps);
            unsafe { b.launch(cfg) }
                .map_err(|e| FcmError::CudaLaunch(format!("divide_numerator kernel: {e}")))?;
        }

        // Step 3: single D2H of fp32 K·d centroids — the only PCIe traffic
        // in this iteration of update_centroids.
        let centroids_host = self
            .stream
            .clone_dtoh(&self.dev_centroids_fp32)
            .map_err(|e| FcmError::CudaLaunch(format!("centroids_fp32 dtoh: {e}")))?;
        for j in 0..k {
            for dim in 0..d {
                centroids_out[[j, dim]] = centroids_host[j * d + dim];
            }
        }
        Ok(())
    }
}

// ============================================================================
// Public CudaFcmBackend
// ============================================================================

pub struct CudaFcmBackend {
    inner: CudaInner,
    n: usize,
    d: usize,
    k: usize,
    precision: GpuPrecision,
}

impl CudaFcmBackend {
    pub fn new(data: &Array2<f32>, k: usize, precision: GpuPrecision) -> Result<Self, FcmError> {
        let n = data.nrows();
        let d = data.ncols();
        if k == 0 || k > n {
            return Err(FcmError::Config(format!(
                "k must be in [1, n]; got k={k}, n={n}"
            )));
        }

        let inner = match precision {
            GpuPrecision::Fp32 => {
                let data_f64 = data.mapv(|v| v as f64);
                CudaInner::Fp32(Fp32State {
                    gpu: GpuFcm::new(&data_f64, k)
                        .map_err(|e| FcmError::CudaInit(format!("fp32: {e}")))?,
                })
            }
            GpuPrecision::Fp16 => CudaInner::Fp16(MixedState::<f16>::new(data, k)?),
            GpuPrecision::Bf16 => CudaInner::Bf16(MixedState::<bf16>::new(data, k)?),
        };

        Ok(Self {
            inner,
            n,
            d,
            k,
            precision,
        })
    }
}

impl FcmBackend for CudaFcmBackend {
    fn n(&self) -> usize {
        self.n
    }

    fn d(&self) -> usize {
        self.d
    }

    fn name(&self) -> &'static str {
        match self.precision {
            GpuPrecision::Fp32 => "cuda-fp32",
            GpuPrecision::Fp16 => "cuda-fp16",
            GpuPrecision::Bf16 => "cuda-bf16",
        }
    }

    fn compute_distances(
        &mut self,
        centroids: &Array2<f32>,
        dist_sq_out: &mut Array2<f32>,
    ) -> Result<(), FcmError> {
        if centroids.nrows() != self.k || centroids.ncols() != self.d {
            return Err(FcmError::Config(format!(
                "centroids shape ({}, {}) != (k={}, d={})",
                centroids.nrows(),
                centroids.ncols(),
                self.k,
                self.d
            )));
        }
        if dist_sq_out.nrows() != self.n || dist_sq_out.ncols() != self.k {
            return Err(FcmError::Config(format!(
                "dist_sq_out shape ({}, {}) != (n={}, k={})",
                dist_sq_out.nrows(),
                dist_sq_out.ncols(),
                self.n,
                self.k
            )));
        }

        match &mut self.inner {
            CudaInner::Fp32(s) => {
                let centroids_f64 = centroids.mapv(|v| v as f64);
                let d2 = s
                    .gpu
                    .compute_distances(&centroids_f64)
                    .map_err(|e| FcmError::CudaLaunch(format!("fp32 distances: {e}")))?;
                for i in 0..self.n {
                    for j in 0..self.k {
                        dist_sq_out[[i, j]] = d2[[i, j]] as f32;
                    }
                }
                Ok(())
            }
            CudaInner::Fp16(s) => s.compute_distances(centroids, dist_sq_out),
            CudaInner::Bf16(s) => s.compute_distances(centroids, dist_sq_out),
        }
    }

    fn update_centroids(
        &mut self,
        u_pow_m: &Array2<f32>,
        centroids_out: &mut Array2<f32>,
    ) -> Result<(), FcmError> {
        if u_pow_m.nrows() != self.n || u_pow_m.ncols() != self.k {
            return Err(FcmError::Config(format!(
                "u_pow_m shape ({}, {}) != (n={}, k={})",
                u_pow_m.nrows(),
                u_pow_m.ncols(),
                self.n,
                self.k
            )));
        }
        if centroids_out.nrows() != self.k || centroids_out.ncols() != self.d {
            return Err(FcmError::Config(format!(
                "centroids_out shape ({}, {}) != (k={}, d={})",
                centroids_out.nrows(),
                centroids_out.ncols(),
                self.k,
                self.d
            )));
        }

        match &mut self.inner {
            CudaInner::Fp32(s) => {
                let u_f64 = u_pow_m.mapv(|v| v as f64);
                let new_c = s
                    .gpu
                    .update_centroids(&u_f64)
                    .map_err(|e| FcmError::CudaLaunch(format!("fp32 update: {e}")))?;
                for j in 0..self.k {
                    for dim in 0..self.d {
                        centroids_out[[j, dim]] = new_c[[j, dim]] as f32;
                    }
                }
                Ok(())
            }
            CudaInner::Fp16(s) => s.update_centroids(u_pow_m, centroids_out),
            CudaInner::Bf16(s) => s.update_centroids(u_pow_m, centroids_out),
        }
    }
}
