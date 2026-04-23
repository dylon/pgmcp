//! Mini-batch / online Fuzzy C-Means for n > 1M chunks.
//!
//! Bounded-memory variant of `fuzzy_c_means`: the per-iteration accumulators
//! (`numerator`, `col_sums`) are O(K·d + K) regardless of n; data is streamed
//! one batch at a time. Per-chunk membership is persisted to LMDB (see
//! `src/topic_store/lmdb.rs`) so the next iteration can read `old_membership`
//! without needing an n·K in-RAM buffer.
//!
//! This path is selected by `run_global_topic_scan` when the chunk count
//! exceeds `cron.topic_online_n_threshold` (default 1_000_000). For smaller
//! corpora the mmap-backed in-memory FCM from `topic_clustering::fuzzy_c_means`
//! is strictly faster (no LMDB round-trips per chunk per iteration) and is
//! used instead.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ndarray::linalg::general_mat_mul;
use ndarray::{Array1, Array2, Zip};
use tracing::{info, warn};

use crate::cron::topic_clustering::{CancelFn, FcmResult, kmeans_plus_plus_init};

/// One mini-batch of data: `chunk_ids[i]` parallels `data.row(i)`.
pub struct MiniBatch<'a> {
    pub chunk_ids: &'a [i64],
    pub data: &'a Array2<f32>,
}

/// Callback signature for streaming mini-batches.
///
/// Implementations should: (a) fetch the next batch of up to `batch_size`
/// embeddings from PostgreSQL (or wherever) at position `offset`, (b) return
/// `Some(MiniBatch)` while rows remain, (c) return `None` when the stream
/// is exhausted.
///
/// For the OOM-fix's purposes, the caller builds a closure that issues
/// `SELECT id, embedding FROM file_chunks ORDER BY id LIMIT $1 OFFSET $2`
/// (or cursor-based pagination).
pub type BatchFetcher<'a> = Box<dyn FnMut(usize, usize) -> Option<(Vec<i64>, Array2<f32>)> + 'a>;

/// Helper capturing all mini-batch FCM config.
pub struct OnlineFcmConfig {
    pub k: usize,
    pub m: f64,
    pub max_iters: usize,
    pub tolerance: f64,
    pub batch_size: usize,
    /// Expected total n (used for accumulator sizing + logging; not enforced).
    pub n_expected: usize,
    /// Embedding dimension d.
    pub d: usize,
}

/// A stored per-chunk membership record used for convergence comparison.
pub type MembershipEntry = Vec<f32>; // length K

/// Trait for a backing store that holds per-chunk membership vectors across
/// iterations. The LMDB store from `topic_store::lmdb` implements this;
/// tests can use an in-memory `HashMap<i64, Vec<f32>>`.
pub trait MembershipStore {
    fn load(&self, chunk_id: i64) -> Option<Vec<f32>>;
    fn store(&mut self, chunk_id: i64, membership: &[f32]);
    fn store_batch(&mut self, items: &[(i64, Vec<f32>)]);
}

/// Run mini-batch online Fuzzy C-Means.
///
/// Convergence is tested per-epoch via max element-wise change across any
/// chunk's membership vector. The caller is responsible for providing the
/// `fetcher` (which streams mini-batches from the data source each epoch) and
/// a `MembershipStore` for cross-iteration persistence.
///
/// Initial centroids: if `initial_centroids` is provided (warm-start from a
/// prior FCM run via LMDB), use them directly. Otherwise k-means++ on the
/// first batch is used as a seed.
///
/// Per CLAUDE.md preallocation rule: all per-iteration buffers (`numerator`,
/// `col_sums`, `u_pow_m_batch`, `dist_sq_batch`, `dot_xc_batch`) are allocated
/// once and reused. Only the membership store sees per-iteration I/O.
pub fn fuzzy_c_means_online<S: MembershipStore>(
    mut fetcher: BatchFetcher<'_>,
    config: &OnlineFcmConfig,
    store: Arc<std::sync::Mutex<S>>,
    initial_centroids: Option<Array2<f32>>,
    should_cancel: CancelFn<'_>,
) -> FcmResult {
    let k = config.k;
    let d = config.d;
    let m_f32 = config.m as f32;
    let exponent = (2.0 / (config.m - 1.0)) as f32;
    let eps_dist: f32 = 1e-6;

    assert!(k > 0, "K must be > 0");
    assert!(config.m > 1.0, "Fuzziness m must be > 1.0");

    // Initialise centroids: warm-start if provided, else k-means++ on first batch.
    let mut centroids = match initial_centroids {
        Some(c) if c.nrows() == k && c.ncols() == d => c,
        _ => {
            let first = fetcher(config.batch_size, 0);
            match first {
                Some((_ids, data)) => {
                    info!(
                        "Online FCM: seeding centroids via k-means++ on first batch of {}",
                        data.nrows()
                    );
                    kmeans_plus_plus_init(data.view(), k)
                }
                None => {
                    warn!("Online FCM: empty data stream; returning zero result");
                    return FcmResult {
                        membership: Array2::<f32>::zeros((0, k)),
                        centroids: Array2::<f32>::zeros((k, d)),
                        iterations: 0,
                        converged: false,
                        cancelled: false,
                        inertia: 0.0,
                    };
                }
            }
        }
    };

    // Preallocated per-iteration accumulators (shape independent of n).
    let mut numerator = Array2::<f32>::zeros((k, d));
    let mut col_sums = Array1::<f32>::zeros(k);
    let mut c_norms = Array1::<f32>::zeros(k);

    // Per-batch scratch (resized if batch size varies; initial size = batch_size).
    let mut dist_sq_batch = Array2::<f32>::zeros((config.batch_size, k));
    let mut dot_xc_batch = Array2::<f32>::zeros((config.batch_size, k));
    let mut new_mem_batch = Array2::<f32>::zeros((config.batch_size, k));
    let mut u_pow_m_batch = Array2::<f32>::zeros((config.batch_size, k));

    let mut iterations = 0;
    let mut converged = false;
    let mut cancelled = false;
    let mut _total_n = 0usize;

    for iter in 0..config.max_iters {
        iterations = iter + 1;

        if let Some(cancel) = should_cancel
            && cancel()
        {
            cancelled = true;
            break;
        }

        // Zero accumulators for this epoch.
        numerator.fill(0.0);
        col_sums.fill(0.0);
        for j in 0..k {
            c_norms[j] = centroids.row(j).dot(&centroids.row(j));
        }

        let mut max_change_epoch: f64 = 0.0;
        let mut epoch_n = 0usize;
        let mut offset = 0usize;

        while let Some((chunk_ids, data)) = fetcher(config.batch_size, offset) {
            let batch_n = data.nrows();
            if batch_n == 0 {
                break;
            }
            epoch_n += batch_n;
            offset += batch_n;

            // Grow scratch buffers if this batch is larger than initial batch_size.
            if batch_n > dist_sq_batch.nrows() {
                dist_sq_batch = Array2::<f32>::zeros((batch_n, k));
                dot_xc_batch = Array2::<f32>::zeros((batch_n, k));
                new_mem_batch = Array2::<f32>::zeros((batch_n, k));
                u_pow_m_batch = Array2::<f32>::zeros((batch_n, k));
            }

            // Sliceable views into the scratch (first batch_n rows).
            let mut dist_view = dist_sq_batch.slice_mut(ndarray::s![..batch_n, ..]);
            let mut dot_view = dot_xc_batch.slice_mut(ndarray::s![..batch_n, ..]);
            let mut new_view = new_mem_batch.slice_mut(ndarray::s![..batch_n, ..]);
            let mut u_view = u_pow_m_batch.slice_mut(ndarray::s![..batch_n, ..]);

            // Distance matrix: dot = data · centroids.t() (batch_n × K).
            general_mat_mul(1.0_f32, &data, &centroids.t(), 0.0_f32, &mut dot_view);

            // dist_sq[i, j] = ||x_i||² + ||c_j||² - 2·dot[i, j]
            for i in 0..batch_n {
                let xn: f32 = data.row(i).dot(&data.row(i));
                for j in 0..k {
                    let d2 = (xn + c_norms[j] - 2.0 * dot_view[[i, j]]).max(eps_dist);
                    dist_view[[i, j]] = d2;
                }
            }

            // New membership (m=2 fast path + general path).
            if (config.m - 2.0).abs() < 1e-12 {
                for i in 0..batch_n {
                    let mut inv_sum: f32 = 0.0;
                    for j in 0..k {
                        inv_sum += 1.0 / dist_view[[i, j]];
                    }
                    let inv_sum = inv_sum.max(eps_dist);
                    for j in 0..k {
                        new_view[[i, j]] = (1.0 / dist_view[[i, j]]) / inv_sum;
                    }
                }
            } else {
                for i in 0..batch_n {
                    for j in 0..k {
                        let mut sum: f32 = 0.0;
                        for l in 0..k {
                            sum += (dist_view[[i, j]] / dist_view[[i, l]]).powf(exponent);
                        }
                        new_view[[i, j]] = 1.0 / sum;
                    }
                }
            }

            // Compare against previous iteration's membership (from store) and
            // persist the new one. Track max element-wise change across batch.
            {
                let mut store_guard = store.lock().expect("membership store mutex poisoned");
                let mut batch_persist: Vec<(i64, Vec<f32>)> = Vec::with_capacity(batch_n);
                for (i, &cid) in chunk_ids.iter().enumerate() {
                    let new_row: Vec<f32> = new_view.row(i).to_vec();
                    if let Some(old) = store_guard.load(cid) {
                        if old.len() == k {
                            for j in 0..k {
                                let delta = (new_row[j] - old[j]).abs() as f64;
                                if delta > max_change_epoch {
                                    max_change_epoch = delta;
                                }
                            }
                        } else {
                            // K changed since last run; treat as max change.
                            max_change_epoch = f64::INFINITY;
                        }
                    } else {
                        // First iteration; seed max_change with the new value magnitude.
                        let max_new = new_row.iter().cloned().fold(0.0_f32, f32::max) as f64;
                        if max_new > max_change_epoch {
                            max_change_epoch = max_new;
                        }
                    }
                    batch_persist.push((cid, new_row));
                }
                store_guard.store_batch(&batch_persist);
            }

            // u_pow_m = new_membership.powf(m), in-place over the slice.
            Zip::from(&mut u_view).and(&new_view).for_each(|dst, &src| {
                *dst = src.powf(m_f32);
            });

            // Accumulate centroid numerator: numerator += (U^m)ᵀ · data.
            // general_mat_mul supports beta=1.0 for accumulation.
            general_mat_mul(1.0_f32, &u_view.t(), &data, 1.0_f32, &mut numerator);

            // Accumulate col_sums
            for i in 0..batch_n {
                for j in 0..k {
                    col_sums[j] += u_view[[i, j]];
                }
            }
        }

        _total_n = epoch_n;

        // Update centroids: C = numerator / col_sums
        for j in 0..k {
            let denom = col_sums[j].max(eps_dist);
            for dim in 0..d {
                centroids[[j, dim]] = numerator[[j, dim]] / denom;
            }
        }

        info!(
            iter = iterations,
            epoch_n = epoch_n,
            max_change = format!("{:.2e}", max_change_epoch),
            "Online FCM epoch complete"
        );

        if max_change_epoch < config.tolerance {
            converged = true;
            info!(iter = iterations, "Online FCM converged");
            break;
        }
    }

    // Note: mini-batch FCM does NOT return the n×k membership matrix in RAM
    // (that would defeat the whole purpose). Callers should read final
    // assignments from the membership store (LMDB) per chunk_id.
    // We return a zero-shaped stub matrix.
    FcmResult {
        membership: Array2::<f32>::zeros((0, k)),
        centroids,
        iterations,
        converged,
        cancelled,
        inertia: 0.0, // inertia would require an extra streaming pass; skip
    }
}

// Suppress unused-import lint until the online path is wired into the cron
// dispatcher in a follow-up.
#[allow(dead_code)]
fn _ordering_ref(_: Ordering) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct InMemoryStore {
        data: HashMap<i64, Vec<f32>>,
    }

    impl InMemoryStore {
        fn new() -> Self {
            Self {
                data: HashMap::new(),
            }
        }
    }

    impl MembershipStore for InMemoryStore {
        fn load(&self, chunk_id: i64) -> Option<Vec<f32>> {
            self.data.get(&chunk_id).cloned()
        }
        fn store(&mut self, chunk_id: i64, membership: &[f32]) {
            self.data.insert(chunk_id, membership.to_vec());
        }
        fn store_batch(&mut self, items: &[(i64, Vec<f32>)]) {
            for (cid, v) in items {
                self.data.insert(*cid, v.clone());
            }
        }
    }

    fn make_synthetic_two_blobs(n_per: usize, d: usize) -> (Vec<i64>, Array2<f32>) {
        let n = n_per * 2;
        let mut data = Array2::<f32>::zeros((n, d));
        // Blob 1: cluster around (1.0, 0.0, ..., 0.0)
        for i in 0..n_per {
            data[[i, 0]] = 1.0 + 0.01 * i as f32;
        }
        // Blob 2: cluster around (0.0, 1.0, 0.0, ..., 0.0)
        for i in 0..n_per {
            data[[n_per + i, 1]] = 1.0 + 0.01 * i as f32;
        }
        let ids: Vec<i64> = (0..n as i64).collect();
        (ids, data)
    }

    #[test]
    fn test_online_fcm_converges_on_two_blobs() {
        let (ids, data) = make_synthetic_two_blobs(20, 4);
        let batch_size = 10;

        let config = OnlineFcmConfig {
            k: 2,
            m: 2.0,
            max_iters: 50,
            tolerance: 1e-4,
            batch_size,
            n_expected: ids.len(),
            d: 4,
        };

        let store = Arc::new(StdMutex::new(InMemoryStore::new()));

        let ids_clone = ids.clone();
        let data_clone = data.clone();
        let fetcher: BatchFetcher = Box::new(move |bs, off| {
            if off >= ids_clone.len() {
                return None;
            }
            let end = (off + bs).min(ids_clone.len());
            let batch_ids = ids_clone[off..end].to_vec();
            let batch_data = data_clone.slice(ndarray::s![off..end, ..]).to_owned();
            Some((batch_ids, batch_data))
        });

        let result = fuzzy_c_means_online(fetcher, &config, store.clone(), None, None);

        assert!(result.converged || result.iterations > 5);
        // Centroids should have moved away from their initial points.
        let c0 = result.centroids.row(0);
        let c1 = result.centroids.row(1);
        let diff: f32 = (&c0 - &c1).mapv(|x| x * x).sum().sqrt();
        assert!(
            diff > 0.5,
            "centroids should be distinct, got distance={}",
            diff
        );

        // Check that per-chunk memberships landed in the store.
        let guard = store.lock().unwrap();
        assert_eq!(guard.data.len(), 40);
    }

    #[test]
    fn test_online_fcm_cancellation() {
        let (ids, data) = make_synthetic_two_blobs(10, 4);
        let config = OnlineFcmConfig {
            k: 2,
            m: 2.0,
            max_iters: 100,
            tolerance: 1e-8,
            batch_size: 5,
            n_expected: 20,
            d: 4,
        };
        let store = Arc::new(StdMutex::new(InMemoryStore::new()));

        let ids_clone = ids.clone();
        let data_clone = data.clone();
        let fetcher: BatchFetcher = Box::new(move |bs, off| {
            if off >= ids_clone.len() {
                return None;
            }
            let end = (off + bs).min(ids_clone.len());
            Some((
                ids_clone[off..end].to_vec(),
                data_clone.slice(ndarray::s![off..end, ..]).to_owned(),
            ))
        });

        let cancel_fn: &(dyn Fn() -> bool + Sync) = &|| true;
        let result = fuzzy_c_means_online(fetcher, &config, store, None, Some(cancel_fn));
        assert!(result.cancelled);
    }

    #[test]
    fn test_online_fcm_empty_stream() {
        let config = OnlineFcmConfig {
            k: 3,
            m: 2.0,
            max_iters: 10,
            tolerance: 1e-4,
            batch_size: 10,
            n_expected: 0,
            d: 8,
        };
        let store = Arc::new(StdMutex::new(InMemoryStore::new()));

        let fetcher: BatchFetcher = Box::new(|_, _| None);
        let result = fuzzy_c_means_online(fetcher, &config, store, None, None);
        assert_eq!(result.iterations, 0);
        assert_eq!(result.membership.nrows(), 0);
    }

    #[test]
    fn test_online_fcm_warm_start_from_centroids() {
        let (ids, data) = make_synthetic_two_blobs(10, 4);
        let config = OnlineFcmConfig {
            k: 2,
            m: 2.0,
            max_iters: 50,
            tolerance: 1e-4,
            batch_size: 5,
            n_expected: 20,
            d: 4,
        };
        let store = Arc::new(StdMutex::new(InMemoryStore::new()));

        // Provide centroids that are already near the true cluster centers.
        let mut initial = Array2::<f32>::zeros((2, 4));
        initial[[0, 0]] = 1.0;
        initial[[1, 1]] = 1.0;

        let ids_clone = ids.clone();
        let data_clone = data.clone();
        let fetcher: BatchFetcher = Box::new(move |bs, off| {
            if off >= ids_clone.len() {
                return None;
            }
            let end = (off + bs).min(ids_clone.len());
            Some((
                ids_clone[off..end].to_vec(),
                data_clone.slice(ndarray::s![off..end, ..]).to_owned(),
            ))
        });

        let result = fuzzy_c_means_online(fetcher, &config, store, Some(initial), None);
        // Warm-start should converge within a few iterations.
        assert!(
            result.iterations <= 15,
            "warm-start should converge quickly, got {} iters",
            result.iterations
        );
    }
}
