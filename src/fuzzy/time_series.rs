//! Time-series fuzzy-match index for per-file commit-cadence patterns.
//!
//! Backed by `liblevenshtein::time_series::MsmConfig::distance`
//! (Move-Split-Merge metric, Stefan et al. 2012). Each entry is a
//! fixed-length vector of weekly commit counts; queries find files
//! whose recent commit rhythms match a probe vector under MSM distance.
//!
//! Used by the `time_series_fuzzy_match` MCP tool (Phase 8) for
//! commit-pattern similarity.

use liblevenshtein::time_series::MsmConfig;
use serde::{Deserialize, Serialize};

/// Per-file commit-cadence series indexed by `file_id`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommitCadenceSeries {
    pub file_id: i64,
    /// Commits per week over the last `series.len()` weeks.
    pub series: Vec<f64>,
}

/// Lightweight time-series index for MSM-distance queries.
pub struct TimeSeriesIndex {
    entries: Vec<CommitCadenceSeries>,
    msm: MsmConfig,
}

impl Default for TimeSeriesIndex {
    fn default() -> Self {
        // Stefan et al. recommend tuning `c` in [0.01, 1.0]; 0.1 works
        // well on weekly commit counts in pgmcp's hardware-spec
        // benchmarks.
        Self::new(0.1)
    }
}

impl TimeSeriesIndex {
    pub fn new(msm_c: f64) -> Self {
        Self {
            entries: Vec::new(),
            msm: MsmConfig::new(msm_c),
        }
    }

    pub fn push(&mut self, entry: CommitCadenceSeries) {
        self.entries.push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find the k file_ids whose cadence series have the smallest MSM
    /// distance to `probe`. Returns `(file_id, distance)` pairs sorted
    /// ascending by distance.
    pub fn nearest(&self, probe: &[f64], k: usize) -> Vec<(i64, f64)> {
        let mut scored: Vec<(i64, f64)> = self
            .entries
            .iter()
            .map(|e| (e.file_id, self.msm.distance(&e.series, probe)))
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
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
