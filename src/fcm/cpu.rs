//! CPU-only FCM backend.
//!
//! Used only as a runtime fallback when CUDA init fails. The two GEMMs run
//! through `ndarray::linalg::general_mat_mul`, which dispatches to Intel
//! MKL via the `blas-src` feature wiring.

use ndarray::{Array1, Array2, Axis, linalg::general_mat_mul};

use super::{FcmBackend, FcmError};

/// Holds the data matrix and all reusable BLAS scratch buffers.
///
/// Construction cost: one pass to compute `‖xᵢ‖²`. Per-iteration cost: two
/// `sgemm` calls plus an elementwise reduction / divide — no allocations.
pub struct CpuFcmBackend {
    data: Array2<f32>,
    /// ‖xᵢ‖² (length n).
    x_norms: Array1<f32>,
    /// Scratch: S = X · Cᵀ (n × K), reused across iterations.
    dot_xc: Array2<f32>,
    /// Scratch: (Uᵐ)ᵀ · X (K × d), reused across iterations.
    numerator: Array2<f32>,
    /// Scratch: ‖cⱼ‖² (length K), recomputed each distance call.
    c_norms: Array1<f32>,
    n: usize,
    d: usize,
    k: usize,
}

impl CpuFcmBackend {
    pub fn new(data: Array2<f32>, k: usize) -> Result<Self, FcmError> {
        let n = data.nrows();
        let d = data.ncols();
        if k == 0 || k > n {
            return Err(FcmError::Config(format!(
                "k must be in [1, n]; got k={k}, n={n}"
            )));
        }

        let x_norms: Array1<f32> = data.map_axis(Axis(1), |row| row.dot(&row));
        Ok(Self {
            data,
            x_norms,
            dot_xc: Array2::<f32>::zeros((n, k)),
            numerator: Array2::<f32>::zeros((k, d)),
            c_norms: Array1::<f32>::zeros(k),
            n,
            d,
            k,
        })
    }
}

impl FcmBackend for CpuFcmBackend {
    fn n(&self) -> usize {
        self.n
    }

    fn d(&self) -> usize {
        self.d
    }

    fn name(&self) -> &'static str {
        "cpu"
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

        // S = X · Cᵀ (n × K), into preallocated scratch.
        general_mat_mul(
            1.0_f32,
            &self.data,
            &centroids.t(),
            0.0_f32,
            &mut self.dot_xc,
        );

        // Recompute ‖cⱼ‖².
        for j in 0..self.k {
            self.c_norms[j] = centroids.row(j).dot(&centroids.row(j));
        }

        // D²ᵢⱼ = max(‖xᵢ‖² + ‖cⱼ‖² − 2·Sᵢⱼ, ε).
        let eps: f32 = 1e-8;
        for i in 0..self.n {
            let xn = self.x_norms[i];
            for j in 0..self.k {
                let d2 = (xn + self.c_norms[j] - 2.0 * self.dot_xc[[i, j]]).max(eps);
                dist_sq_out[[i, j]] = d2;
            }
        }
        Ok(())
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

        // numerator = (Uᵐ)ᵀ · X (K × d).
        general_mat_mul(
            1.0_f32,
            &u_pow_m.t(),
            &self.data,
            0.0_f32,
            &mut self.numerator,
        );

        // col_sums[j] = Σᵢ uᵢⱼᵐ, then centroid = numerator / col_sums.
        let eps: f32 = 1e-8;
        for j in 0..self.k {
            let mut col_sum: f32 = 0.0;
            for i in 0..self.n {
                col_sum += u_pow_m[[i, j]];
            }
            let denom = col_sum.max(eps);
            for dim in 0..self.d {
                centroids_out[[j, dim]] = self.numerator[[j, dim]] / denom;
            }
        }
        Ok(())
    }
}
