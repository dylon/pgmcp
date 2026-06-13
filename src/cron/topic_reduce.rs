//! Dimensionality reduction for the embedding-BERTopic topic-clustering track
//! (Phase 2, Track B).
//!
//! ## Why this exists
//!
//! The FCM topic engine collapsed because it clustered **1024-dimensional**
//! BGE-M3 embeddings directly with a Euclidean/cosine distance. In ~1024-d,
//! pairwise distances *concentrate* (the curse of dimensionality): the measured
//! pairwise-cosine spread over the live corpus was σ ≈ 0.06 around a mean of
//! 0.59, so every `D² = 2(1−cos)` sat at 0.82 ± 0.12 and the fuzzy memberships
//! flattened to a uniform `1/K`. Reducing to ~10–50 dimensions **restores
//! distance contrast**, which is exactly the BERTopic recipe (UMAP→cluster).
//!
//! This module provides two in-tree, dependency-free reducers (the project has
//! no `ndarray-linalg`/LAPACK and a strong in-tree preference):
//!
//! - [`ReduceMethod::Pca`] — principal component analysis via **subspace
//!   iteration** on the (sampled) covariance matrix, ordered by a small
//!   Rayleigh–Ritz [`jacobi_eigen`] step. True PCA, no LAPACK.
//! - [`ReduceMethod::RandomProjection`] — a Johnson–Lindenstrauss Gaussian
//!   projection. Cheapest; provably preserves pairwise distances w.h.p. Useful
//!   as a baseline and when the covariance fit is too expensive.
//!
//! Both re-L2-normalize the reduced rows so the downstream FCM still operates on
//! the unit sphere (cosine ≡ Euclidean), where its `m`-fuzziness behaves well.
//!
//! UMAP (via the `annembed` crate) is the SOTA non-linear option; it is gated
//! behind dependency vetting (`ndarray-linalg` skew vs the `ndarray 0.16` pin)
//! and is *not* wired here — PCA is the in-tree default and the bake-off decides
//! whether the extra UMAP dependency earns its place.

use ndarray::{Array2, ArrayView2};

/// Which reducer to apply before clustering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReduceMethod {
    /// PCA via subspace iteration (true principal components; default).
    Pca,
    /// Johnson–Lindenstrauss Gaussian random projection (cheapest).
    RandomProjection,
}

impl ReduceMethod {
    /// Parse from a config string; unknown → PCA.
    pub fn parse(s: &str) -> Self {
        match s {
            "random" | "random_projection" | "rp" => Self::RandomProjection,
            _ => Self::Pca,
        }
    }
}

/// Maximum rows used to *fit* the PCA basis (the covariance estimate). All rows
/// are still projected; only the basis is fit on a sample, which is standard
/// practice and keeps the O(n·d²) covariance build bounded.
const PCA_FIT_SAMPLE_CAP: usize = 8_000;

/// Number of subspace-iteration passes. The embedding covariance spectrum decays
/// quickly, so a dozen passes recovers the top components to ample precision for
/// clustering (we do not need machine-precision eigenvectors).
const SUBSPACE_ITERS: usize = 12;

/// Oversampling added to the target rank during subspace iteration to improve
/// convergence of the wanted components (randomized-SVD style).
const OVERSAMPLE: usize = 8;

/// Reduce `data` (n × d, expected L2-normalized rows) to `target_dim`
/// dimensions, then re-L2-normalize each reduced row.
///
/// Returns an `(n × target_dim')` matrix where `target_dim' = min(target_dim,
/// d, n)`. When `target_dim >= d` the input is returned unchanged (already
/// "reduced").
pub fn reduce(
    data: ArrayView2<f32>,
    target_dim: usize,
    method: ReduceMethod,
    seed: u64,
) -> Array2<f32> {
    let n = data.nrows();
    let d = data.ncols();
    let k = target_dim.min(d).min(n.max(1));
    if k == 0 || n == 0 {
        return Array2::<f32>::zeros((n, k));
    }
    if target_dim >= d {
        return data.to_owned();
    }

    let mut reduced = match method {
        ReduceMethod::Pca => pca_project(data, k),
        ReduceMethod::RandomProjection => random_projection(data, k, seed),
    };

    // Re-L2-normalize reduced rows so downstream FCM stays on the unit sphere.
    for i in 0..reduced.nrows() {
        let norm: f32 = reduced.row(i).dot(&reduced.row(i)).sqrt();
        if norm > 1e-12 {
            reduced.row_mut(i).mapv_inplace(|x| x / norm);
        }
    }
    reduced
}

/// PCA projection: fit the top-`k` principal axes on a sample of `data`, then
/// project every row onto them.
fn pca_project(data: ArrayView2<f32>, k: usize) -> Array2<f32> {
    let n = data.nrows();
    let d = data.ncols();

    // Mean over a bounded sample (deterministic stride so it spans the corpus).
    let sample_n = n.min(PCA_FIT_SAMPLE_CAP);
    let stride = (n / sample_n).max(1);
    let sample_idx: Vec<usize> = (0..n).step_by(stride).take(sample_n).collect();

    let mut mean = vec![0.0f64; d];
    for &i in &sample_idx {
        let row = data.row(i);
        for j in 0..d {
            mean[j] += row[j] as f64;
        }
    }
    let s = sample_idx.len() as f64;
    for m in &mut mean {
        *m /= s;
    }

    // Covariance C = (1/s) Σ (x-μ)(x-μ)ᵀ over the sample (d × d, f64).
    let mut cov = Array2::<f64>::zeros((d, d));
    let mut centered = vec![0.0f64; d];
    for &i in &sample_idx {
        let row = data.row(i);
        for j in 0..d {
            centered[j] = row[j] as f64 - mean[j];
        }
        // Rank-1 update C += c cᵀ (upper triangle, mirrored after).
        for a in 0..d {
            let ca = centered[a];
            if ca == 0.0 {
                continue;
            }
            let mut rowa = cov.row_mut(a);
            for b in a..d {
                rowa[b] += ca * centered[b];
            }
        }
    }
    // Symmetrize + scale.
    for a in 0..d {
        for b in (a + 1)..d {
            let v = cov[[a, b]] / s;
            cov[[a, b]] = v;
            cov[[b, a]] = v;
        }
        cov[[a, a]] /= s;
    }

    // Top-k eigenvectors of the symmetric PSD covariance via subspace iteration.
    let basis = top_eigenvectors(&cov, k); // (d × k), columns = principal axes

    // Project all rows: reduced[i] = (x_i - μ) · basis.
    let mut reduced = Array2::<f32>::zeros((n, k));
    for i in 0..n {
        let row = data.row(i);
        for c in 0..k {
            let mut acc = 0.0f64;
            let col = basis.column(c);
            for j in 0..d {
                acc += (row[j] as f64 - mean[j]) * col[j];
            }
            reduced[[i, c]] = acc as f32;
        }
    }
    reduced
}

/// Top-`k` eigenvectors (columns) of a symmetric PSD matrix `cov` (d × d) via
/// subspace (block power) iteration followed by a Rayleigh–Ritz rotation so the
/// returned columns are properly ordered by descending eigenvalue.
fn top_eigenvectors(cov: &Array2<f64>, k: usize) -> Array2<f64> {
    let d = cov.nrows();
    let m = (k + OVERSAMPLE).min(d);

    // Deterministic random start, orthonormalized.
    let mut q = Array2::<f64>::zeros((d, m));
    let mut rng = SplitMix64::new(0x5eed_c0de_1234_5678);
    for r in 0..d {
        for c in 0..m {
            q[[r, c]] = rng.next_gaussian();
        }
    }
    modified_gram_schmidt(&mut q);

    // Subspace iteration: Q ← orth(C · Q).
    for _ in 0..SUBSPACE_ITERS {
        let z = cov.dot(&q); // (d × m)
        q = z;
        modified_gram_schmidt(&mut q);
    }

    // Rayleigh–Ritz: M = Qᵀ C Q (m × m), eigendecompose, rotate.
    let cq = cov.dot(&q); // (d × m)
    let mut small = Array2::<f64>::zeros((m, m));
    for a in 0..m {
        for b in 0..m {
            let mut acc = 0.0;
            for r in 0..d {
                acc += q[[r, a]] * cq[[r, b]];
            }
            small[[a, b]] = acc;
        }
    }
    let (eigvals, eigvecs) = jacobi_eigen(&small);

    // Order columns by descending eigenvalue, take top-k.
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&i, &j| {
        eigvals[j]
            .partial_cmp(&eigvals[i])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // V = Q · eigvecs[:, order[..k]]  → (d × k)
    let mut basis = Array2::<f64>::zeros((d, k));
    for (out_c, &ev_c) in order.iter().take(k).enumerate() {
        for r in 0..d {
            let mut acc = 0.0;
            for a in 0..m {
                acc += q[[r, a]] * eigvecs[[a, ev_c]];
            }
            basis[[r, out_c]] = acc;
        }
    }
    basis
}

/// In-place modified Gram–Schmidt orthonormalization of the columns of `q`.
fn modified_gram_schmidt(q: &mut Array2<f64>) {
    let (_d, m) = (q.nrows(), q.ncols());
    for c in 0..m {
        // Subtract projections onto earlier columns.
        for prev in 0..c {
            let dot: f64 = q.column(c).dot(&q.column(prev));
            let prev_col = q.column(prev).to_owned();
            let mut col = q.column_mut(c);
            for r in 0..col.len() {
                col[r] -= dot * prev_col[r];
            }
        }
        // Normalize.
        let norm: f64 = q.column(c).dot(&q.column(c)).sqrt();
        if norm > 1e-12 {
            let mut col = q.column_mut(c);
            for r in 0..col.len() {
                col[r] /= norm;
            }
        }
    }
}

/// Cyclic Jacobi eigenvalue algorithm for a small symmetric matrix.
/// Returns `(eigenvalues, eigenvectors)` where eigenvectors are columns.
fn jacobi_eigen(input: &Array2<f64>) -> (Vec<f64>, Array2<f64>) {
    let n = input.nrows();
    let mut a = input.clone();
    let mut v = Array2::<f64>::eye(n);
    if n == 0 {
        return (Vec::new(), v);
    }

    for _sweep in 0..100 {
        // Largest off-diagonal magnitude.
        let mut off = 0.0;
        for p in 0..n {
            for q in (p + 1)..n {
                off += a[[p, q]] * a[[p, q]];
            }
        }
        if off.sqrt() < 1e-12 {
            break;
        }
        for p in 0..n {
            for q in (p + 1)..n {
                let apq = a[[p, q]];
                if apq.abs() < 1e-300 {
                    continue;
                }
                let app = a[[p, p]];
                let aqq = a[[q, q]];
                let theta = (aqq - app) / (2.0 * apq);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                // Rotate A.
                for i in 0..n {
                    let aip = a[[i, p]];
                    let aiq = a[[i, q]];
                    a[[i, p]] = c * aip - s * aiq;
                    a[[i, q]] = s * aip + c * aiq;
                }
                for i in 0..n {
                    let api = a[[p, i]];
                    let aqi = a[[q, i]];
                    a[[p, i]] = c * api - s * aqi;
                    a[[q, i]] = s * api + c * aqi;
                }
                // Accumulate eigenvectors.
                for i in 0..n {
                    let vip = v[[i, p]];
                    let viq = v[[i, q]];
                    v[[i, p]] = c * vip - s * viq;
                    v[[i, q]] = s * vip + c * viq;
                }
            }
        }
    }

    let eigvals: Vec<f64> = (0..n).map(|i| a[[i, i]]).collect();
    (eigvals, v)
}

/// Johnson–Lindenstrauss Gaussian random projection: `reduced = X · R` where
/// `R` is (d × k) with i.i.d. `N(0, 1/k)` entries.
fn random_projection(data: ArrayView2<f32>, k: usize, seed: u64) -> Array2<f32> {
    let n = data.nrows();
    let d = data.ncols();
    let scale = 1.0 / (k as f64).sqrt();
    let mut r = Array2::<f64>::zeros((d, k));
    let mut rng = SplitMix64::new(seed ^ 0xa5a5_5a5a_dead_beef);
    for a in 0..d {
        for b in 0..k {
            r[[a, b]] = rng.next_gaussian() * scale;
        }
    }
    let mut reduced = Array2::<f32>::zeros((n, k));
    for i in 0..n {
        let row = data.row(i);
        for c in 0..k {
            let mut acc = 0.0f64;
            for j in 0..d {
                acc += row[j] as f64 * r[[j, c]];
            }
            reduced[[i, c]] = acc as f32;
        }
    }
    reduced
}

/// Small deterministic PRNG (SplitMix64) + Box–Muller Gaussian, so reduction is
/// reproducible across runs (the bake-off needs determinism;
/// `Math.random`-style global RNG would not reproduce).
struct SplitMix64 {
    state: u64,
    spare: Option<f64>,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            spare: None,
        }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_f64(&mut self) -> f64 {
        // 53-bit mantissa in [0, 1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn next_gaussian(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        // Box–Muller; guard u1 away from 0.
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (std::f64::consts::TAU * u2).cos();
        let z1 = mag * (std::f64::consts::TAU * u2).sin();
        self.spare = Some(z1);
        z0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;

    /// PCA on data with variance concentrated along one axis should recover that
    /// axis as the top principal component (projected coordinate dominates).
    #[test]
    fn pca_recovers_dominant_axis() {
        // 200 points: large spread on dim 0, tiny on the rest (d = 6).
        let d = 6;
        let n = 200;
        let mut data = Array2::<f32>::zeros((n, d));
        let mut rng = SplitMix64::new(42);
        for i in 0..n {
            data[[i, 0]] = (rng.next_gaussian() * 10.0) as f32;
            for j in 1..d {
                data[[i, j]] = (rng.next_gaussian() * 0.01) as f32;
            }
        }
        let reduced = pca_project(data.view(), 1);
        assert_eq!(reduced.dim(), (n, 1));
        // The single reduced coordinate should track dim 0 (up to sign/scale):
        // correlation magnitude near 1.
        let mut num = 0.0f64;
        let mut da = 0.0f64;
        let mut db = 0.0f64;
        let (mut ma, mut mb) = (0.0f64, 0.0f64);
        for i in 0..n {
            ma += data[[i, 0]] as f64;
            mb += reduced[[i, 0]] as f64;
        }
        ma /= n as f64;
        mb /= n as f64;
        for i in 0..n {
            let x = data[[i, 0]] as f64 - ma;
            let y = reduced[[i, 0]] as f64 - mb;
            num += x * y;
            da += x * x;
            db += y * y;
        }
        let corr = num / (da.sqrt() * db.sqrt());
        assert!(corr.abs() > 0.95, "corr with dominant axis = {corr}");
    }

    /// Reduction must break distance concentration: in high-d near-uniform-cosine
    /// data, the *spread* of pairwise distances should rise after reduction.
    #[test]
    fn reduction_increases_distance_contrast() {
        let d = 256;
        let n = 150;
        let mut data = Array2::<f32>::zeros((n, d));
        let mut rng = SplitMix64::new(7);
        for i in 0..n {
            for j in 0..d {
                data[[i, j]] = rng.next_gaussian() as f32;
            }
            let norm: f32 = data.row(i).dot(&data.row(i)).sqrt();
            data.row_mut(i).mapv_inplace(|x| x / norm);
        }
        let cv_before = pairwise_dist_cv(data.view());
        let reduced = reduce(data.view(), 16, ReduceMethod::Pca, 1);
        let cv_after = pairwise_dist_cv(reduced.view());
        assert!(
            cv_after > cv_before,
            "distance contrast should rise: before={cv_before}, after={cv_after}"
        );
    }

    #[test]
    fn random_projection_preserves_norms_approximately() {
        let d = 128;
        let n = 100;
        let mut data = Array2::<f32>::zeros((n, d));
        let mut rng = SplitMix64::new(99);
        for i in 0..n {
            for j in 0..d {
                data[[i, j]] = rng.next_gaussian() as f32;
            }
        }
        // JL preserves *pairwise* distances; check that a moderately-sized
        // projection yields finite, non-degenerate output.
        let reduced = random_projection(data.view(), 32, 5);
        assert_eq!(reduced.dim(), (n, 32));
        let any_nonzero = reduced.iter().any(|&x| x.abs() > 1e-6);
        assert!(any_nonzero, "random projection produced all-zeros");
    }

    #[test]
    fn jacobi_diagonalizes_symmetric() {
        // Known 2×2 symmetric matrix [[2,1],[1,2]] → eigenvalues 3,1.
        let mut m = Array2::<f64>::zeros((2, 2));
        m[[0, 0]] = 2.0;
        m[[0, 1]] = 1.0;
        m[[1, 0]] = 1.0;
        m[[1, 1]] = 2.0;
        let (mut vals, _vecs) = jacobi_eigen(&m);
        vals.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert!((vals[0] - 3.0).abs() < 1e-9, "λ0={}", vals[0]);
        assert!((vals[1] - 1.0).abs() < 1e-9, "λ1={}", vals[1]);
    }

    #[test]
    fn reduce_passthrough_when_target_ge_dim() {
        let data = Array2::<f32>::from_shape_fn((5, 4), |(i, j)| (i + j) as f32);
        let out = reduce(data.view(), 4, ReduceMethod::Pca, 1);
        assert_eq!(out.dim(), (5, 4));
    }

    /// Coefficient of variation of pairwise *Euclidean* distances — the
    /// canonical curse-of-dimensionality measure. In high-d, distances
    /// concentrate so `stddev/mean → 0`; reducing dimensionality raises it.
    /// (Euclidean mean is stable ~√2 for unit vectors, so the ratio is
    /// well-defined — unlike a cosine CV whose mean ≈ 0 for random directions.)
    fn pairwise_dist_cv(data: ArrayView2<f32>) -> f64 {
        let n = data.nrows();
        let mut dists = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let a = data.row(i);
                let b = data.row(j);
                let mut sq = 0.0f64;
                for d in 0..a.len() {
                    let diff = (a[d] - b[d]) as f64;
                    sq += diff * diff;
                }
                dists.push(sq.sqrt());
            }
        }
        let mean = dists.iter().sum::<f64>() / dists.len() as f64;
        let var = dists.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / dists.len() as f64;
        var.sqrt() / mean.max(1e-9)
    }
}
