//! PTX loader for the fused-reduction CUDA kernels.
//!
//! The kernels are compiled at `cargo build` time by `build.rs` via `nvcc
//! --gpu-architecture=compute_89`, producing `$OUT_DIR/fcm_kernels.ptx`.
//! We embed that PTX into the binary with `include_str!`, then `ctx.load_module`
//! it at backend-construction time. The driver JITs compute_89 PTX → sm_89
//! SASS at load-time (~100 µs).

use std::sync::{Arc, OnceLock};

use cudarc::driver::{CudaContext, CudaFunction, CudaModule};
use cudarc::nvrtc::Ptx;

const FCM_KERNELS_PTX: &str = include_str!(concat!(env!("OUT_DIR"), "/fcm_kernels.ptx"));

/// The eight functions exported by `kernels.cu`, loaded once per FCM backend.
///
/// The first four (distance reduction + on-device c_norms) are Phase 5–11
/// kernels. The last four (col-sums reduction + numerator divide) are
/// Phase 10 — they replace `MixedState::update_centroids`'s host-side
/// col-sums sweep and per-element divide with on-device kernels, leaving
/// only one D2H per iteration (the fp32 K·d centroids).
pub struct FcmKernels {
    pub fused_distance_reduce_fp16: CudaFunction,
    pub fused_distance_reduce_bf16: CudaFunction,
    pub compute_c_norms_fp16: CudaFunction,
    pub compute_c_norms_bf16: CudaFunction,
    pub reduce_col_sums_fp16: CudaFunction,
    pub reduce_col_sums_bf16: CudaFunction,
    pub divide_numerator_by_col_sums_fp16: CudaFunction,
    pub divide_numerator_by_col_sums_bf16: CudaFunction,
}

impl FcmKernels {
    /// Load all kernels into the given CUDA context. The underlying
    /// `CudaModule` is shared across loads via a process-wide cache so
    /// subsequent FCM runs don't re-parse the PTX.
    pub fn load(ctx: &Arc<CudaContext>) -> Result<Self, cudarc::driver::DriverError> {
        let module = cached_module(ctx)?;
        Ok(Self {
            fused_distance_reduce_fp16: module.load_function("fused_distance_reduce_fp16")?,
            fused_distance_reduce_bf16: module.load_function("fused_distance_reduce_bf16")?,
            compute_c_norms_fp16: module.load_function("compute_c_norms_fp16")?,
            compute_c_norms_bf16: module.load_function("compute_c_norms_bf16")?,
            reduce_col_sums_fp16: module.load_function("reduce_col_sums_fp16")?,
            reduce_col_sums_bf16: module.load_function("reduce_col_sums_bf16")?,
            divide_numerator_by_col_sums_fp16: module
                .load_function("divide_numerator_by_col_sums_fp16")?,
            divide_numerator_by_col_sums_bf16: module
                .load_function("divide_numerator_by_col_sums_bf16")?,
        })
    }
}

/// (context_id, module) entry stored by `cached_module`. Lifting the complex
/// type behind an alias keeps `clippy::type_complexity` happy.
type ModuleCacheEntry = Option<(usize, Arc<CudaModule>)>;

/// Per-process cache of the compiled kernel module. CudaModule is per-context
/// in cudarc's API, but in practice we only ever use ctx 0 (`CudaContext::new(0)`);
/// a single cell is sufficient. If a future caller creates multiple contexts,
/// they'll each re-load the module — still correct, just less cached.
fn cached_module(ctx: &Arc<CudaContext>) -> Result<Arc<CudaModule>, cudarc::driver::DriverError> {
    static CACHE: OnceLock<std::sync::Mutex<ModuleCacheEntry>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| std::sync::Mutex::new(None));

    let ctx_id = Arc::as_ptr(ctx) as usize;
    let mut guard = cell.lock().expect("cached_module mutex poisoned");
    if let Some((cached_id, module)) = guard.as_ref()
        && *cached_id == ctx_id
    {
        return Ok(Arc::clone(module));
    }

    let ptx = Ptx::from_src(FCM_KERNELS_PTX.to_string());
    let module = ctx.load_module(ptx)?;
    *guard = Some((ctx_id, Arc::clone(&module)));
    Ok(module)
}
