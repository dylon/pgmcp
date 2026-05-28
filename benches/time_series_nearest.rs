//! Criterion bench (B1): naive O(n) pairwise MSM scan vs liblevenshtein's
//! admissible lower-bound-pruned, rayon-parallel range search in
//! `TimeSeriesIndex::nearest`.
//!
//! The LB-pruned path is the same exact-MSM engine `TrajectoryIndex` already
//! uses; the lower bound is admissible, so both produce identical k-NN results.
//! This bench is the data-driven gate for the B1 change (see the persistent-trie
//! review plan): keep the change only if retrieval is faster-or-neutral on
//! realistic per-file commit-cadence collections.
//!
//! Run pinned to stable cores for reproducible numbers, e.g.:
//!   taskset -c 0-7 cargo bench --bench time_series_nearest

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use liblevenshtein::time_series::MsmConfig;
use pgmcp::fuzzy::time_series::{CommitCadenceSeries, TimeSeriesIndex};

/// Deterministic synthetic commit-cadence DB: `n` files, each a `len`-week
/// series of small integer weekly commit counts in `[0, 11]`. Reproducible via
/// an inline LCG so the bench is stable across runs without depending on a PRNG
/// crate's evolving API.
fn synth_db(n: usize, len: usize) -> Vec<(i64, Vec<f64>)> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 40) % 12) as f64
    };
    let mut db = Vec::with_capacity(n);
    for i in 0..n {
        let mut series = Vec::with_capacity(len);
        for _ in 0..len {
            series.push(next());
        }
        db.push((i as i64, series));
    }
    db
}

/// The pre-B1 baseline: exhaustive pairwise MSM scan, sort, truncate.
fn naive_nearest(
    db: &[(i64, Vec<f64>)],
    probe: &[f64],
    k: usize,
    msm: &MsmConfig,
) -> Vec<(i64, f64)> {
    let mut scored: Vec<(i64, f64)> = db
        .iter()
        .map(|(id, s)| (*id, msm.distance(s, probe)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

fn bench_nearest(c: &mut Criterion) {
    const WEEKS: usize = 26; // ~6 months of weekly commit counts
    const K: usize = 10;
    let msm = MsmConfig::new(0.1);

    for &n in &[1_000usize, 10_000, 50_000] {
        let db = synth_db(n, WEEKS);
        let probe = db[n / 2].1.clone(); // an in-distribution probe

        let mut idx = TimeSeriesIndex::new(0.1);
        for (id, s) in &db {
            idx.push(CommitCadenceSeries {
                file_id: *id,
                series: s.clone(),
            });
        }

        let mut group = c.benchmark_group(format!("time_series_nearest_n{n}"));
        group.bench_function("naive_scan", |b| {
            b.iter(|| black_box(naive_nearest(black_box(&db), black_box(&probe), K, &msm)))
        });
        group.bench_function("lb_pruned_parallel", |b| {
            b.iter(|| black_box(idx.nearest(black_box(&probe), K)))
        });
        group.finish();
    }
}

criterion_group!(benches, bench_nearest);
criterion_main!(benches);
