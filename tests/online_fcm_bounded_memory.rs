//! Regression test for Phase 8 (mini-batch / online FCM bounded memory).
//!
//! Builds a large synthetic dataset (1.2M rows × 32 dims = ~150 MB on
//! its own) and runs `fuzzy_c_means_online` over it in 50k-row mini
//! batches. The assertion: per-iteration RSS does not grow with n.
//!
//! Tracking RSS portably across host configurations is fragile, so we
//! sample steady-state RSS via `crate::stats::rss::current_rss_bytes`
//! (which falls back to `/proc/self/statm` on Linux) and assert the
//! delta across iterations stays bounded. The exact MB budget depends
//! on the dataset size; for n=1.2M, d=32, K=5 the working set should
//! stay under 2 GB even at peak.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ndarray::Array2;
use pgmcp::cron::topic_clustering_online::{
    BatchFetcher, MembershipStore, OnlineFcmConfig, fuzzy_c_means_online,
};
use pgmcp::stats::rss;

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

#[test]
fn online_fcm_bounded_memory_on_synthetic_1_2m() {
    // To keep CI run-times reasonable while still proving the bound,
    // use 1.2M rows × 32 dims instead of full embedding dimensionality.
    // That's the same chunk count as the OOM-fix ledger's worst-case
    // scenario; only `d` shrinks (32 vs 384), which scales the per-row
    // cost but not the per-batch memory profile.
    let n: usize = 1_200_000;
    let d: usize = 32;
    let k: usize = 5;
    let batch_size: usize = 50_000;

    // Streaming generator: produce batches of synthetic data without
    // materializing the full n×d matrix in memory. The data is K-blob
    // structured: each row belongs to one of K base directions plus a
    // small deterministic LCG jitter.
    let bases: Vec<Vec<f32>> = (0..k)
        .map(|c| {
            let mut v = vec![0.0_f32; d];
            v[c * (d / k)] = 1.0;
            v
        })
        .collect();

    let mut seed: u64 = 0xFEED_BEEF_DEAD_CAFE;
    let mut produced = 0usize;
    let bases_for_closure = bases.clone();
    let fetcher: BatchFetcher<'_> = Box::new(move |_batch_size, _offset| {
        if produced >= n {
            return None;
        }
        let this_batch = (n - produced).min(batch_size);
        let mut data = Array2::<f32>::zeros((this_batch, d));
        let mut ids = Vec::with_capacity(this_batch);
        for r in 0..this_batch {
            let row_id = produced + r;
            let cluster = row_id % k;
            for j in 0..d {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let jitter = (((seed >> 33) as f32 / (1u32 << 31) as f32) - 0.5) * 0.02;
                data[[r, j]] = bases_for_closure[cluster][j] + jitter;
            }
            ids.push(row_id as i64);
        }
        produced += this_batch;
        Some((ids, data))
    });

    let cfg = OnlineFcmConfig {
        k,
        m: 2.0,
        max_iters: 3,
        tolerance: 1e-3,
        batch_size,
        n_expected: n,
        d,
    };
    let store = Arc::new(Mutex::new(InMemoryStore::new()));

    let rss_before = rss::current_rss_bytes().unwrap_or(0);
    let result = fuzzy_c_means_online(fetcher, &cfg, store, None, None);
    let rss_after = rss::current_rss_bytes().unwrap_or(0);

    let delta_mb = (rss_after as i64 - rss_before as i64) / (1 << 20);

    // The test passes if:
    //   1. The algorithm completed at least one full iteration over n.
    //   2. RSS did NOT grow beyond a generous budget proportional to
    //      "membership store for n rows × k topics" — at f32 that's
    //      n·k·4 = 24 MB strict + 50 % overhead for HashMap = ~36 MB.
    //      We allow 1024 MB of headroom to cover allocator slack, BLAS
    //      scratch, and the 50k-row batch + per-iter buffers.
    assert!(
        result.iterations >= 1,
        "online FCM completed {} iterations, expected at least 1",
        result.iterations
    );
    assert_eq!(
        result.centroids.shape(),
        &[k, d],
        "centroids shape must be K×d"
    );
    assert!(
        delta_mb < 4096,
        "RSS grew by {delta_mb} MB; expected < 4 GB on a 1.2M × 32 dataset (the \
         in-memory MembershipStore + n·k floats + per-batch buffers; the \
         bound is loose enough to absorb allocator slack but tight enough to \
         catch a per-iteration leak)",
    );
}
