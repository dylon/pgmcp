//! Time-series fuzzy-match index for per-file commit-cadence patterns.
//!
//! Backed by `liblevenshtein::time_series::MsmConfig::distance`
//! (Move-Split-Merge metric, Stefan et al. 2012). Each entry is a
//! fixed-length vector of weekly commit counts; queries find files
//! whose recent commit rhythms match a probe vector under MSM distance.
//!
//! Used by the `time_series_fuzzy_match` MCP tool (Phase 8) for
//! commit-pattern similarity.

use liblevenshtein::time_series::{MsmConfig, search_with_lb_parallel};
use serde::{Deserialize, Serialize};

/// Per-file commit-cadence series indexed by `file_id`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommitCadenceSeries {
    pub file_id: i64,
    /// Commits per week over the last `series.len()` weeks.
    pub series: Vec<f64>,
}

/// Default split/merge cost (Stefan et al. recommend `c ∈ [0.01, 1.0]`).
const DEFAULT_MSM_C: f64 = 0.1;
/// Upper bound for the expanding-threshold k-NN; guards against an unbounded
/// loop when the index holds fewer than `k` entries.
const MAX_THRESHOLD: f64 = 1.0e9;

/// Lightweight time-series index for MSM-distance queries.
///
/// Series are stored as `(file_id, series)` rows so retrieval can use
/// liblevenshtein's admissible lower-bound-pruned, rayon-parallel range search
/// (`search_with_lb_parallel`) — the same exact-MSM engine [`TrajectoryIndex`]
/// uses — instead of a brute-force pairwise scan. The lower bound is admissible,
/// so results are identical to the exhaustive scan, just with far fewer full MSM
/// evaluations on all but the nearest candidates.
///
/// [`TrajectoryIndex`]: crate::fuzzy::trajectory_index::TrajectoryIndex
pub struct TimeSeriesIndex {
    db: Vec<(i64, Vec<f64>)>,
    msm: MsmConfig,
}

impl Default for TimeSeriesIndex {
    fn default() -> Self {
        // Stefan et al. recommend tuning `c` in [0.01, 1.0]; 0.1 works
        // well on weekly commit counts in pgmcp's hardware-spec benchmarks.
        Self::new(DEFAULT_MSM_C)
    }
}

impl TimeSeriesIndex {
    pub fn new(msm_c: f64) -> Self {
        Self {
            db: Vec::new(),
            msm: MsmConfig::new(msm_c),
        }
    }

    pub fn push(&mut self, entry: CommitCadenceSeries) {
        self.db.push((entry.file_id, entry.series));
    }

    pub fn len(&self) -> usize {
        self.db.len()
    }

    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    /// Find the `k` file_ids whose cadence series have the smallest MSM distance
    /// to `probe`, as `(file_id, distance)` pairs sorted ascending by distance.
    ///
    /// Uses an expanding-threshold range search over liblevenshtein's admissible
    /// lower-bound-pruned parallel MSM (`search_with_lb_parallel`): each round is
    /// exact within its threshold, and doubling the threshold guarantees at least
    /// `k` results unless the index holds fewer than `k`. The seed threshold is
    /// scaled to the probe so the first round usually already contains `k`
    /// neighbors. Results are identical to an exhaustive scan (the lower bound
    /// never discards a true neighbor).
    pub fn nearest(&self, probe: &[f64], k: usize) -> Vec<(i64, f64)> {
        if self.db.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut threshold = self.seed_threshold(probe);
        loop {
            // Hits within `threshold`, already sorted ascending by distance.
            let mut hits = search_with_lb_parallel(probe, &self.db, threshold, &self.msm);
            if hits.len() >= k || threshold >= MAX_THRESHOLD {
                hits.truncate(k);
                return hits;
            }
            threshold *= 2.0;
        }
    }

    /// A scale-aware initial threshold so the first range search usually already
    /// contains `k` neighbors (avoids many doubling rounds). Mirrors
    /// `TrajectoryIndex::seed_threshold`.
    fn seed_threshold(&self, probe: &[f64]) -> f64 {
        let scale = probe.iter().fold(0.0_f64, |m, x| m.max(x.abs())).max(1.0);
        scale * (probe.len().max(1) as f64) * 0.1 + 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_picks_closest_pattern() {
        let mut idx = TimeSeriesIndex::new(0.1);
        idx.push(CommitCadenceSeries {
            file_id: 1,
            series: vec![1.0, 2.0, 3.0, 2.0, 1.0],
        });
        idx.push(CommitCadenceSeries {
            file_id: 2,
            series: vec![1.0, 2.0, 3.0, 2.0, 1.0],
        });
        idx.push(CommitCadenceSeries {
            file_id: 3,
            series: vec![10.0, 0.0, 0.0, 0.0, 10.0],
        });
        let probe = vec![1.0, 2.0, 3.0, 2.0, 1.0];
        let near = idx.nearest(&probe, 2);
        assert_eq!(near.len(), 2);
        // file_id 1 or 2 should be the closest (distance 0 to identical series).
        assert!(near.iter().any(|(id, _)| *id == 1 || *id == 2));
    }
}
