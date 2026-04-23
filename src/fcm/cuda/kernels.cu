// Fused post-GEMM distance reduction for the FCM CUDA backend.
//
// For each (i, j) in [0, n) × [0, K):
//   dist_sq[i, j] = max(x_norms[i] + c_norms[j] - 2 * dot[i, j], eps)
//
// `dot` is the row-major (n × K) output of cuBLAS `X · Cᵀ` stored in fp16
// or bf16. `x_norms[i] = ‖xᵢ‖²` and `c_norms[j] = ‖cⱼ‖²` are precomputed
// in fp32 — x_norms once at backend construction, c_norms per-iteration
// via `compute_c_norms_*` below.
//
// Output is always fp32 — matches the FcmBackend trait signature and keeps
// downstream membership arithmetic in a clean precision.
//
// Each output element is independent (no reduction across threads), so we
// use one block per row i and let threads stride along j.

#include <cuda_bf16.h>
#include <cuda_fp16.h>

extern "C" {

__global__ void fused_distance_reduce_fp16(
    const __half* __restrict__ dot,     // (n × K), fp16, row-major
    const float*  __restrict__ x_norms, // (n), fp32
    const float*  __restrict__ c_norms, // (K), fp32
    float*        __restrict__ dist_sq, // (n × K), fp32, output
    const int     n,
    const int     k,
    const float   eps)
{
    const int i = blockIdx.x;
    if (i >= n) return;
    const int row_off = i * k;
    const float xn = x_norms[i];

    for (int j = threadIdx.x; j < k; j += blockDim.x) {
        const float s  = __half2float(dot[row_off + j]);
        const float cn = c_norms[j];
        float d2 = xn + cn - 2.0f * s;
        if (d2 < eps) d2 = eps;
        dist_sq[row_off + j] = d2;
    }
}

__global__ void fused_distance_reduce_bf16(
    const __nv_bfloat16* __restrict__ dot,
    const float*  __restrict__ x_norms,
    const float*  __restrict__ c_norms,
    float*        __restrict__ dist_sq,
    const int     n,
    const int     k,
    const float   eps)
{
    const int i = blockIdx.x;
    if (i >= n) return;
    const int row_off = i * k;
    const float xn = x_norms[i];

    for (int j = threadIdx.x; j < k; j += blockDim.x) {
        const float s  = __bfloat162float(dot[row_off + j]);
        const float cn = c_norms[j];
        float d2 = xn + cn - 2.0f * s;
        if (d2 < eps) d2 = eps;
        dist_sq[row_off + j] = d2;
    }
}

// On-device ‖cⱼ‖² computation. One block per j; block-stride reduction
// across the d dimension with shared-memory accumulation. Block size 128.
//
// The centroids buffer lives on the device (uploaded by the backend's
// compute_distances path); doing this norm on-device avoids a separate
// host→device round-trip for c_norms each iteration.

__global__ void compute_c_norms_fp16(
    const __half* __restrict__ centroids, // (K × d), fp16, row-major
    float*        __restrict__ c_norms,   // (K), fp32, output
    const int     k,
    const int     d)
{
    const int j = blockIdx.x;
    if (j >= k) return;
    const int row_off = j * d;

    __shared__ float partial[128];
    float acc = 0.0f;
    for (int t = threadIdx.x; t < d; t += blockDim.x) {
        const float v = __half2float(centroids[row_off + t]);
        acc += v * v;
    }
    partial[threadIdx.x] = acc;
    __syncthreads();

    for (int stride = blockDim.x / 2; stride > 0; stride >>= 1) {
        if (threadIdx.x < stride) {
            partial[threadIdx.x] += partial[threadIdx.x + stride];
        }
        __syncthreads();
    }

    if (threadIdx.x == 0) {
        c_norms[j] = partial[0];
    }
}

__global__ void compute_c_norms_bf16(
    const __nv_bfloat16* __restrict__ centroids,
    float*        __restrict__ c_norms,
    const int     k,
    const int     d)
{
    const int j = blockIdx.x;
    if (j >= k) return;
    const int row_off = j * d;

    __shared__ float partial[128];
    float acc = 0.0f;
    for (int t = threadIdx.x; t < d; t += blockDim.x) {
        const float v = __bfloat162float(centroids[row_off + t]);
        acc += v * v;
    }
    partial[threadIdx.x] = acc;
    __syncthreads();

    for (int stride = blockDim.x / 2; stride > 0; stride >>= 1) {
        if (threadIdx.x < stride) {
            partial[threadIdx.x] += partial[threadIdx.x + stride];
        }
        __syncthreads();
    }

    if (threadIdx.x == 0) {
        c_norms[j] = partial[0];
    }
}

} // extern "C"
