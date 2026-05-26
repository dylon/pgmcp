//! MSM trajectory-similarity index (Part B4).
//!
//! An RLM run is recorded as a sequence of typed steps, encoded as one
//! f64 per step (see `crate::a2a::rlm::encode_step`). This module compares
//! those sequences with the **Move-Split-Merge** metric — a true metric
//! over `&[f64]` (Stefan et al.) — to (a) retrieve the most similar past
//! runs and (b) classify whether a run trends toward success or failure.
//!
//! Retrieval is exact: `search_with_lb_parallel` is an admissible
//! lower-bound-pruned, rayon-parallel **range** search; we wrap it in an
//! expanding-threshold k-NN. The split/merge cost `c` is adaptive —
//! [`calibrate_adaptive_c`] tunes it to the trajectory distribution.
//!
//! Adaptive-c uses the real Follow-the-Perturbed-Tropical-Leader learner
//! `adaptive_msm::AdaptiveMsm` (FPTL). It lives in the `adaptive-msm` crate
//! — extracted from liblevenshtein's `wfst::msm` to break the
//! `liblevenshtein → lling-llang → liblevenshtein` package cycle — and the
//! learner depends only on the base `MsmConfig` (no lling-llang), so pgmcp
//! consumes it with `default-features = false`.

use adaptive_msm::{AdaptiveMsm, AdaptiveMsmConfig};
use liblevenshtein::time_series::{MsmConfig, search_with_lb_parallel};
use sqlx::PgPool;

/// Default split/merge cost (Stefan et al. recommend `c ∈ [0.01, 1.0]`).
pub const DEFAULT_MSM_C: f64 = 0.1;
/// Upper bound for the expanding-threshold k-NN (guards against an
/// unbounded loop when the database has fewer than `k` entries).
const MAX_THRESHOLD: f64 = 1.0e9;

/// Load the persisted adaptive MSM cost `c` from `pgmcp_metadata` (shared by
/// the RLM strategy chooser, the `trajectory_similarity` tool, and the
/// calibration cron).
pub(crate) async fn load_msm_c(pool: &PgPool) -> Option<f64> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM pgmcp_metadata WHERE key = 'a2a_msm_adaptive_c'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    row.and_then(|(s,)| s.parse::<f64>().ok())
}

/// Persist the adaptive MSM cost `c`.
pub(crate) async fn store_msm_c(pool: &PgPool, c: f64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('a2a_msm_adaptive_c', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(c.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// An in-memory MSM index over `(trajectory_id, encoded_series)` rows.
pub struct TrajectoryIndex {
    db: Vec<(i64, Vec<f64>)>,
    msm: MsmConfig,
}

impl TrajectoryIndex {
    /// Build from rows with an explicit split/merge cost `c`.
    pub fn new(rows: Vec<(i64, Vec<f64>)>, c: f64) -> Self {
        let c = if c > 0.0 { c } else { DEFAULT_MSM_C };
        Self {
            db: rows,
            msm: MsmConfig::new(c),
        }
    }

    pub fn len(&self) -> usize {
        self.db.len()
    }

    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    /// Exact k-NN by expanding-threshold range search. Each round is exact
    /// within its threshold; doubling guarantees ≥`k` results unless the
    /// database holds fewer than `k` (after exclusion). `exclude_id` drops
    /// the probe's own row (self-match) when probing by `task_id`.
    pub fn nearest(&self, probe: &[f64], k: usize, exclude_id: Option<i64>) -> Vec<(i64, f64)> {
        if self.db.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut threshold = self.seed_threshold(probe);
        loop {
            let hits = search_with_lb_parallel(probe, &self.db, threshold, &self.msm);
            // hits are already sorted ascending by distance.
            let mut filtered: Vec<(i64, f64)> = hits
                .into_iter()
                .filter(|(id, _)| Some(*id) != exclude_id)
                .collect();
            if filtered.len() >= k || threshold >= MAX_THRESHOLD {
                filtered.truncate(k);
                return filtered;
            }
            threshold *= 2.0;
        }
    }

    /// A scale-aware initial threshold so the first range search usually
    /// already contains `k` neighbors (avoids many doubling rounds).
    fn seed_threshold(&self, probe: &[f64]) -> f64 {
        let scale = probe.iter().fold(0.0_f64, |m, x| m.max(x.abs())).max(1.0);
        scale * (probe.len().max(1) as f64) * 0.1 + 1.0
    }
}

/// Predict whether `probe` trends toward success or failure by comparing
/// the mean MSM distance to its k nearest *successful* trajectories versus
/// its k nearest *failed* ones. Returns
/// `(predicted_success, success_mean_dist, fail_mean_dist)`, or `None` when
/// neither cohort has any labeled rows.
pub fn classify_trend(
    probe: &[f64],
    success_rows: Vec<(i64, Vec<f64>)>,
    fail_rows: Vec<(i64, Vec<f64>)>,
    k: usize,
    c: f64,
) -> Option<(bool, f64, f64)> {
    let success_empty = success_rows.is_empty();
    let fail_empty = fail_rows.is_empty();
    if success_empty && fail_empty {
        return None;
    }
    let s = TrajectoryIndex::new(success_rows, c).nearest(probe, k, None);
    let f = TrajectoryIndex::new(fail_rows, c).nearest(probe, k, None);
    let mean = |v: &[(i64, f64)]| -> f64 {
        if v.is_empty() {
            f64::INFINITY
        } else {
            v.iter().map(|(_, d)| *d).sum::<f64>() / v.len() as f64
        }
    };
    let sm = mean(&s);
    let fm = mean(&f);
    Some((sm <= fm, sm, fm))
}

/// Adaptive split/merge cost `c`, tuned for cohort SEPARATION by the real
/// Follow-the-Perturbed-Tropical-Leader learner (`adaptive_msm::AdaptiveMsm`).
///
/// Each round draws one exploration `c` (`explore_c`) and observes a
/// contrastive hinge loss
/// `max(0, margin + d(query, same-cohort) − d(query, other-cohort))`, so FPTL
/// gradient descent moves `c` toward a value that pulls same-cohort
/// trajectories together and pushes cross-cohort ones apart (the closed-loop
/// objective: a `c` under which the success/failure cohorts are best
/// separated yields a sharper strategy chooser).
///
/// A leave-one-out **precision guard** then adopts the learned `c` only when
/// it *strictly* improves LOO cohort-classification accuracy over
/// `initial_c`; otherwise `initial_c` is kept — calibration never regresses.
/// Falls back to `initial_c` when either cohort is too small (< 2) to define
/// the objective. Deterministic (seeded) and clamped to `[0.01, 1.0]`.
pub fn calibrate_adaptive_c(
    success: &[(i64, Vec<f64>)],
    fail: &[(i64, Vec<f64>)],
    initial_c: f64,
    max_rounds: usize,
) -> f64 {
    let fallback = (if initial_c > 0.0 {
        initial_c
    } else {
        DEFAULT_MSM_C
    })
    .clamp(0.01, 1.0);
    // A separation objective needs ≥2 labeled trajectories per cohort
    // (a query plus at least one distinct same-cohort positive).
    if success.len() < 2 || fail.len() < 2 {
        return fallback;
    }
    let incumbent_acc = loo_accuracy(success, fail, fallback);

    let mut learner = AdaptiveMsm::new(
        AdaptiveMsmConfig::new()
            .initial_c(fallback)
            .epsilon(0.15)
            .c_bounds(0.01, 1.0)
            .window_size(16)
            .seed(0x5151_4d53_4d00),
    );
    const MARGIN: f64 = 0.5;
    let rounds = max_rounds.max(32);
    let mut best_c = fallback;
    let mut best_acc = incumbent_acc;
    for r in 0..rounds {
        // Deterministic contrastive triple, alternating which cohort owns the
        // query so both directions of separation are optimized.
        let (query, positive, negative) = if r % 2 == 0 {
            (
                &success[r % success.len()].1,
                &success[(r + 1) % success.len()].1,
                &fail[r % fail.len()].1,
            )
        } else {
            (
                &fail[r % fail.len()].1,
                &fail[(r + 1) % fail.len()].1,
                &success[r % success.len()].1,
            )
        };
        // ONE exploration c per round, used for BOTH distances (a contrastive
        // round must score positive and negative under the same c).
        let c = learner.explore_c();
        let msm = MsmConfig::new(c);
        let loss =
            (MARGIN + msm.distance(query, positive) - msm.distance(query, negative)).max(0.0);
        learner.observe(loss);
        // Keep the c with the best LOO separation visited this run.
        let acc = loo_accuracy(success, fail, learner.current_c());
        if acc > best_acc {
            best_acc = acc;
            best_c = learner.current_c();
        }
    }
    if best_acc > incumbent_acc {
        best_c.clamp(0.01, 1.0)
    } else {
        fallback
    }
}

/// Leave-one-out cohort-classification accuracy at cost `c`: each labeled
/// trajectory is classified by whether its mean MSM distance to the OTHER
/// members of its own cohort is smaller than to the opposite cohort; returns
/// the fraction classified correctly. This is the precision metric the
/// calibration guard protects.
pub fn loo_accuracy(success: &[(i64, Vec<f64>)], fail: &[(i64, Vec<f64>)], c: f64) -> f64 {
    let msm = MsmConfig::new(c);
    let total = success.len() + fail.len();
    if total == 0 {
        return 0.0;
    }
    let mut correct = 0usize;
    for (id, s) in success {
        if mean_dist_excluding(&msm, s, success, *id) < mean_dist_excluding(&msm, s, fail, i64::MIN)
        {
            correct += 1;
        }
    }
    for (id, s) in fail {
        if mean_dist_excluding(&msm, s, fail, *id) < mean_dist_excluding(&msm, s, success, i64::MIN)
        {
            correct += 1;
        }
    }
    correct as f64 / total as f64
}

/// Mean MSM distance from `probe` to every cohort member except `exclude_id`
/// (the LOO self-exclusion). `∞` when the cohort is empty after exclusion, so
/// a singleton never spuriously classifies as its own cohort.
fn mean_dist_excluding(
    msm: &MsmConfig,
    probe: &[f64],
    cohort: &[(i64, Vec<f64>)],
    exclude_id: i64,
) -> f64 {
    let mut sum = 0.0;
    let mut n = 0usize;
    for (id, series) in cohort {
        if *id == exclude_id {
            continue;
        }
        sum += msm.distance(probe, series);
        n += 1;
    }
    if n == 0 {
        f64::INFINITY
    } else {
        sum / n as f64
    }
}

/// Back-fill `agent_trajectories.success` from high-confidence
/// `agent_outcomes` — the Part-A↔B integration seam (FK join, not fuzzy
/// matching): a run is labeled by the most recent *explicit* outcome about
/// its task. The low-confidence auto-write-back (confidence 0.4) is
/// excluded so labels reflect real signal. Returns rows newly labeled.
pub async fn label_trajectories_from_outcomes(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE agent_trajectories t
            SET success = (ao.outcome IN ('worked','prefer')),
                outcome_obs_id = ao.observation_id
         FROM (
             SELECT DISTINCT ON (parent_task_id)
                    parent_task_id, outcome::text AS outcome, observation_id
             FROM agent_outcomes
             WHERE parent_task_id IS NOT NULL AND confidence >= 0.6
             ORDER BY parent_task_id, created_at DESC
         ) ao
         WHERE ao.parent_task_id = t.task_id AND t.success IS NULL",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(i64, Vec<f64>)> {
        vec![
            (1, vec![1.0, 2.0, 4.0, 6.0]),
            (2, vec![1.0, 2.0, 4.4, 4.4, 6.2]), // near 1
            (3, vec![1.0, 2.0, 3.0, 3.0, 3.0, 6.0]),
            (4, vec![1.0, 2.0, 4.0, 6.0, 6.0]), // near 1
        ]
    }

    #[test]
    fn nearest_returns_k_sorted_by_distance() {
        let idx = TrajectoryIndex::new(rows(), 0.1);
        let probe = vec![1.0, 2.0, 4.0, 6.0];
        let hits = idx.nearest(&probe, 2, None);
        assert_eq!(hits.len(), 2);
        // exact match (id=1) should be nearest (distance ~0).
        assert_eq!(hits[0].0, 1);
        assert!(
            hits[0].1 <= hits[1].1,
            "results sorted ascending by distance"
        );
    }

    #[test]
    fn nearest_excludes_self() {
        let idx = TrajectoryIndex::new(rows(), 0.1);
        let probe = vec![1.0, 2.0, 4.0, 6.0];
        let hits = idx.nearest(&probe, 3, Some(1));
        assert!(hits.iter().all(|(id, _)| *id != 1), "self excluded");
    }

    #[test]
    fn empty_index_returns_empty() {
        let idx = TrajectoryIndex::new(Vec::new(), 0.1);
        assert!(idx.nearest(&[1.0, 2.0], 5, None).is_empty());
    }

    #[test]
    fn classify_trend_prefers_closer_cohort() {
        let success = vec![
            (10, vec![1.0, 2.0, 4.0, 6.0]),
            (11, vec![1.0, 2.0, 4.1, 6.1]),
        ];
        let fail = vec![
            (20, vec![9.0, 9.0, 9.0, 9.0]),
            (21, vec![8.0, 8.5, 9.0, 9.5]),
        ];
        let probe = vec![1.0, 2.0, 4.0, 6.0];
        let (success_pred, sm, fm) = classify_trend(&probe, success, fail, 2, 0.1).expect("trend");
        assert!(success_pred, "probe near success cohort");
        assert!(sm < fm);
    }

    #[test]
    fn calibrate_returns_bounded_c() {
        let success = vec![
            (1, vec![1.0, 2.0, 4.0, 6.0]),
            (2, vec![1.0, 2.0, 4.1, 6.1]),
            (3, vec![1.1, 2.1, 4.0, 6.2]),
        ];
        let fail = vec![
            (4, vec![9.0, 9.0, 9.0, 9.0]),
            (5, vec![8.5, 9.0, 9.2, 9.4]),
            (6, vec![8.8, 9.1, 9.0, 9.3]),
        ];
        let c = calibrate_adaptive_c(&success, &fail, 0.1, 64);
        assert!((0.01..=1.0).contains(&c), "learned c in bounds: {c}");
    }

    #[test]
    fn calibrate_never_regresses_loo_accuracy() {
        // The precision guard: the adopted c's LOO accuracy is ≥ the
        // incumbent's (never regress), and well-separated cohorts classify
        // perfectly under it.
        let success = vec![
            (1, vec![1.0, 2.0, 4.0, 6.0]),
            (2, vec![1.0, 2.0, 4.1, 6.1]),
            (3, vec![1.1, 2.1, 4.0, 6.2]),
        ];
        let fail = vec![
            (4, vec![9.0, 9.0, 9.0, 9.0]),
            (5, vec![8.5, 9.0, 9.2, 9.4]),
            (6, vec![8.8, 9.1, 9.0, 9.3]),
        ];
        let incumbent = loo_accuracy(&success, &fail, 0.1);
        let c = calibrate_adaptive_c(&success, &fail, 0.1, 64);
        assert!(
            loo_accuracy(&success, &fail, c) >= incumbent,
            "guard must not regress LOO accuracy"
        );
    }

    #[test]
    fn calibrate_falls_back_when_cohort_too_small() {
        let success = vec![(1, vec![1.0, 2.0, 4.0, 6.0])];
        let fail = vec![(2, vec![9.0, 9.0, 9.0, 9.0])];
        // Singleton cohorts can't define the separation objective → keep c.
        assert_eq!(calibrate_adaptive_c(&success, &fail, 0.1, 64), 0.1);
    }
}
