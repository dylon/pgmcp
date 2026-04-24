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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DIST_EPS: f32 = 1e-8;

    /// Strategy: (n, d, k) triples + random data matrix (f32 in [-5, 5]).
    fn data_and_k_strategy() -> impl Strategy<Value = (Array2<f32>, usize)> {
        (2usize..15, 1usize..8, 1usize..6).prop_flat_map(|(n, d, k_max)| {
            let k = k_max.min(n);
            prop::collection::vec(-5.0f32..5.0, n * d).prop_map(move |values| {
                let arr = Array2::from_shape_vec((n, d), values).expect("shape matches length");
                (arr, k)
            })
        })
    }

    fn random_centroids(k: usize, d: usize) -> Array2<f32> {
        let values: Vec<f32> = (0..k * d).map(|i| (i as f32).sin()).collect();
        Array2::from_shape_vec((k, d), values).expect("shape")
    }

    fn random_memberships(n: usize, k: usize) -> Array2<f32> {
        // Row-stochastic memberships raised to m=2 → values in [0, 1].
        let mut u = Array2::<f32>::zeros((n, k));
        for i in 0..n {
            let mut total = 0.0;
            for j in 0..k {
                // Deterministic but non-trivial values.
                let v = ((i as f32) * 13.0 + (j as f32) * 7.0).sin().abs() + 0.01;
                u[[i, j]] = v;
                total += v;
            }
            // Normalize row to sum to 1, then square (m=2 after U^m).
            for j in 0..k {
                let normed = u[[i, j]] / total;
                u[[i, j]] = normed * normed;
            }
        }
        u
    }

    proptest! {
        /// Every distance is ≥ ε (never zero, never negative, never NaN).
        #[test]
        fn prop_compute_distances_nonnegative_and_finite(
            (data, k) in data_and_k_strategy(),
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let centroids = random_centroids(k, d);
            let mut dist = Array2::<f32>::zeros((n, k));
            backend.compute_distances(&centroids, &mut dist).expect("distances");
            for &v in dist.iter() {
                prop_assert!(v >= DIST_EPS, "distance {} must be >= epsilon", v);
                prop_assert!(v.is_finite(), "distance {} must be finite", v);
            }
        }

        /// Permuting centroid rows permutes the columns of the distance
        /// matrix correspondingly (D[i, π(j)] = D_orig[i, j]).
        #[test]
        fn prop_compute_distances_symmetric_under_centroid_permutation(
            (data, k) in data_and_k_strategy(),
        ) {
            prop_assume!(k >= 2);
            let n = data.nrows();
            let d = data.ncols();
            let centroids = random_centroids(k, d);

            let mut backend_a = CpuFcmBackend::new(data.clone(), k).expect("backend a");
            let mut dist_a = Array2::<f32>::zeros((n, k));
            backend_a.compute_distances(&centroids, &mut dist_a).expect("da");

            // Swap centroid rows 0 and k-1.
            let mut permuted = centroids.clone();
            {
                let (mut row0, mut row1) = permuted.multi_slice_mut((
                    ndarray::s![0_usize, ..],
                    ndarray::s![k - 1, ..],
                ));
                for col in 0..d {
                    std::mem::swap(&mut row0[col], &mut row1[col]);
                }
            }
            let mut backend_b = CpuFcmBackend::new(data, k).expect("backend b");
            let mut dist_b = Array2::<f32>::zeros((n, k));
            backend_b.compute_distances(&permuted, &mut dist_b).expect("db");

            // dist_b columns 0 and k-1 should be dist_a columns k-1 and 0.
            for i in 0..n {
                prop_assert!((dist_a[[i, 0]] - dist_b[[i, k - 1]]).abs() < 1e-4);
                prop_assert!((dist_a[[i, k - 1]] - dist_b[[i, 0]]).abs() < 1e-4);
            }
        }

        /// Wrong-shape centroids produce an error — no panic, no UB.
        #[test]
        fn prop_compute_distances_rejects_bad_centroid_shape(
            (data, k) in data_and_k_strategy(),
            bad_k_delta in 1usize..3,
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let wrong = random_centroids(k + bad_k_delta, d);
            let mut dist = Array2::<f32>::zeros((n, k));
            let err = backend.compute_distances(&wrong, &mut dist);
            prop_assert!(err.is_err(), "shape mismatch must error");
        }

        /// Distance to a centroid at `data[i]` is ≈ ε (floor), not 0 —
        /// documents the epsilon floor in `compute_distances`.
        #[test]
        fn prop_compute_distances_self_hits_epsilon_floor(
            (data, _k_unused) in data_and_k_strategy(),
        ) {
            // Use the actual row-0 of data as centroid 0.
            let n = data.nrows();
            let k = 1usize;
            let centroids = data.row(0).to_owned().insert_axis(Axis(0));
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let mut dist = Array2::<f32>::zeros((n, k));
            backend.compute_distances(&centroids, &mut dist).expect("distances");
            // d[0, 0] = (‖x0‖² + ‖c0‖² − 2·x0·c0).max(ε) = 0.max(ε) = ε.
            // Floating-point rounding can push it slightly above ε for noisy
            // data — allow a small band.
            let d00 = dist[[0, 0]];
            prop_assert!(d00 < 1e-2, "d(x,x) = {} should be tiny", d00);
        }

        /// Update shape matches the contract (K, d).
        #[test]
        fn prop_update_centroids_preserves_shape(
            (data, k) in data_and_k_strategy(),
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let u_pow_m = random_memberships(n, k);
            let mut out = Array2::<f32>::zeros((k, d));
            backend.update_centroids(&u_pow_m, &mut out).expect("update");
            prop_assert_eq!(out.nrows(), k);
            prop_assert_eq!(out.ncols(), d);
        }

        /// All centroid coordinates are finite.
        #[test]
        fn prop_update_centroids_finite(
            (data, k) in data_and_k_strategy(),
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let u_pow_m = random_memberships(n, k);
            let mut out = Array2::<f32>::zeros((k, d));
            backend.update_centroids(&u_pow_m, &mut out).expect("update");
            for &v in out.iter() {
                prop_assert!(v.is_finite(), "centroid coordinate {} must be finite", v);
            }
        }

        /// Idempotence under repeated application with the same U.
        /// Running `update_centroids` twice with the same memberships must
        /// produce the same output each time (the result only depends on
        /// U and the data, not on prior output state).
        #[test]
        fn prop_update_centroids_idempotent(
            (data, k) in data_and_k_strategy(),
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let u_pow_m = random_memberships(n, k);
            let mut a = Array2::<f32>::zeros((k, d));
            let mut b = Array2::<f32>::zeros((k, d));
            backend.update_centroids(&u_pow_m, &mut a).expect("first");
            backend.update_centroids(&u_pow_m, &mut b).expect("second");
            for ((i, j), &av) in a.indexed_iter() {
                let bv = b[[i, j]];
                prop_assert!((av - bv).abs() < 1e-5,
                    "centroid[{}][{}]: {} vs {}", i, j, av, bv);
            }
        }

        /// Every centroid coordinate lies within [min, max] of the
        /// corresponding data coordinate — since it's a convex combination.
        #[test]
        fn prop_update_centroids_in_data_bounds(
            (data, k) in data_and_k_strategy(),
        ) {
            let n = data.nrows();
            let d = data.ncols();
            let mut dim_min = vec![f32::INFINITY; d];
            let mut dim_max = vec![f32::NEG_INFINITY; d];
            for i in 0..n {
                for j in 0..d {
                    let v = data[[i, j]];
                    dim_min[j] = dim_min[j].min(v);
                    dim_max[j] = dim_max[j].max(v);
                }
            }
            let mut backend = CpuFcmBackend::new(data, k).expect("backend");
            let u_pow_m = random_memberships(n, k);
            let mut out = Array2::<f32>::zeros((k, d));
            backend.update_centroids(&u_pow_m, &mut out).expect("update");
            let tol = 1e-4_f32;
            for j in 0..k {
                for dim in 0..d {
                    let v = out[[j, dim]];
                    prop_assert!(
                        v >= dim_min[dim] - tol && v <= dim_max[dim] + tol,
                        "centroid[{}][{}] = {} outside data range [{}, {}]",
                        j, dim, v, dim_min[dim], dim_max[dim],
                    );
                }
            }
        }
    }

    #[test]
    fn new_rejects_k_zero() {
        let data = Array2::<f32>::zeros((5, 3));
        match CpuFcmBackend::new(data, 0) {
            Ok(_) => panic!("k=0 should error"),
            Err(FcmError::Config(_)) => {}
            Err(e) => panic!("expected Config error, got {:?}", e),
        }
    }

    #[test]
    fn new_rejects_k_greater_than_n() {
        let data = Array2::<f32>::zeros((5, 3));
        match CpuFcmBackend::new(data, 10) {
            Ok(_) => panic!("k>n should error"),
            Err(FcmError::Config(_)) => {}
            Err(e) => panic!("expected Config error, got {:?}", e),
        }
    }
}
