//! `trajectory-similarity` cron (Stage 5c): MSM `evolves_like` edges.
//!
//! Captures *how records evolved over time* — which timestamps/recency and
//! set-based co-change both miss — by comparing per-record numeric
//! **trajectories** with the Move-Split-Merge metric (a true metric over
//! `&[f64]`, Stefan et al.) from `liblevenshtein::time_series`, the same engine
//! the A2A/RLM `TrajectoryIndex` uses. Trajectory sources (extensible — any
//! `(node_id, node_type, series)` plugs into the same MSM-kNN):
//!
//! - **work_item** — the progress-% series (`work_item_progress.percent`
//!   ordered by time);
//! - **file** — the weekly commit-churn series (commits touching the file per
//!   ISO week, from `git_commit_files` ⋈ `git_commits`).
//!
//! For each node type, the top-`max_per_type` most-sampled trajectories are
//! queried k-NN with liblevenshtein's admissible lower-bound-pruned MSM range
//! search, and the nearest `k` (within `max_distance`) become `evolves_like`
//! edges in `trajectory_similarities`, which `memory_unified_edges` surfaces.
//! The split/merge cost `c` is the Stefan-recommended default. Scheduled from
//! `src/cli/daemon.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use liblevenshtein::time_series::{MsmConfig, search_with_lb_parallel};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::TrajectorySimilarityConfig;
use crate::stats::tracker::StatsTracker;

/// Upper bound for expanding-threshold k-NN. Prevents unbounded loops when the
/// cohort contains fewer than the requested number of neighbors.
const MAX_MSM_THRESHOLD: f64 = 1.0e9;

/// One record's numeric trajectory.
struct Traj {
    node_id: String,
    node_type: String,
    series: Vec<f64>,
}

/// work_item progress-% trajectories (≥ `min_points` samples).
async fn work_item_trajectories(pool: &PgPool, min_points: i64) -> Result<Vec<Traj>, sqlx::Error> {
    let rows: Vec<(i64, Vec<i16>)> = sqlx::query_as(
        "SELECT item_id,
                array_agg(percent ORDER BY created_at) FILTER (WHERE percent IS NOT NULL) AS series
         FROM work_item_progress
         GROUP BY item_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ps)| {
            let series: Vec<f64> = ps.into_iter().map(|p| p as f64).collect();
            (series.len() as i64 >= min_points).then(|| Traj {
                node_id: format!("work_item:{id}"),
                node_type: "work_item".to_string(),
                series,
            })
        })
        .collect())
}

/// file weekly-commit-churn trajectories (≥ `min_points` weeks of activity).
async fn file_trajectories(pool: &PgPool, min_points: i64) -> Result<Vec<Traj>, sqlx::Error> {
    let rows: Vec<(i64, Vec<i64>)> = sqlx::query_as(
        "WITH buckets AS (
             SELECT f.id AS file_id,
                    date_trunc('week', gc.author_date) AS wk,
                    count(*)::int8 AS n
             FROM git_commit_files gcf
             JOIN git_commits gc ON gc.id = gcf.commit_id
             JOIN indexed_files f
               ON f.project_id = gc.project_id AND f.relative_path = gcf.file_path
             GROUP BY f.id, date_trunc('week', gc.author_date)
         )
         SELECT file_id, array_agg(n ORDER BY wk) AS series
         FROM buckets
         GROUP BY file_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ns)| {
            let series: Vec<f64> = ns.into_iter().map(|n| n as f64).collect();
            (series.len() as i64 >= min_points).then(|| Traj {
                node_id: format!("file:{id}"),
                node_type: "file".to_string(),
                series,
            })
        })
        .collect())
}

/// deadlock-risk trajectories (ADR-011): per-project `deadlock_cycle_count`
/// series from `concurrency_health_history`. Node id is the registered
/// `project:{id}` unified node; cohort type `deadlock_risk`. So
/// `recognize_trajectory("deadlock_risk", live_counts, …)` surfaces projects
/// whose deadlock-cycle trajectory resembles this one.
async fn deadlock_risk_trajectories(
    pool: &PgPool,
    min_points: i64,
) -> Result<Vec<Traj>, sqlx::Error> {
    let rows: Vec<(i32, Vec<i32>)> = sqlx::query_as(
        "SELECT project_id,
                array_agg(deadlock_cycle_count ORDER BY computed_at) AS series
         FROM concurrency_health_history
         GROUP BY project_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(pid, cs)| {
            let series: Vec<f64> = cs.into_iter().map(|c| c as f64).collect();
            (series.len() as i64 >= min_points).then(|| Traj {
                node_id: format!("project:{pid}"),
                node_type: "deadlock_risk".to_string(),
                series,
            })
        })
        .collect())
}

/// lock-contention trajectories (ADR-011): per-lock contention-score series from
/// `concurrency_health_history.raw_summary->'lock_contention'` across snapshots.
/// Node id is the registered `lock_resource:{key}` unified node; cohort type
/// `lock_contention` — "this lock's contention trajectory resembles ones that
/// historically led to deadlock".
async fn lock_contention_trajectories(
    pool: &PgPool,
    min_points: i64,
) -> Result<Vec<Traj>, sqlx::Error> {
    let rows: Vec<(String, Vec<f64>)> = sqlx::query_as(
        "WITH expanded AS (
             SELECT h.computed_at, e.key AS lock_key, e.value::float8 AS score
             FROM concurrency_health_history h,
                  LATERAL jsonb_each_text(h.raw_summary -> 'lock_contention') AS e(key, value)
             WHERE h.raw_summary ? 'lock_contention'
         )
         SELECT lock_key, array_agg(score ORDER BY computed_at) AS series
         FROM expanded
         GROUP BY lock_key",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, series)| {
            (series.len() as i64 >= min_points).then(|| Traj {
                node_id: format!("lock_resource:{key}"),
                node_type: "lock_contention".to_string(),
                series,
            })
        })
        .collect())
}

/// MSM k-NN within one node type's trajectory cohort. Returns edge tuples
/// `(from_id, from_type, to_id, to_type, weight, msm_distance)`.
fn msm_knn(
    trajs: &[Traj],
    c: f64,
    max_distance: f64,
    k: usize,
) -> Vec<(String, String, String, String, f64, f64)> {
    if trajs.is_empty() || k == 0 || max_distance.is_nan() || max_distance < 0.0 {
        return Vec::new();
    }

    let msm = MsmConfig::new(c);
    let db = trajectory_db(trajs);
    let mut out = Vec::new();
    for (i, a) in trajs.iter().enumerate() {
        let hits = search_with_lb_parallel(&a.series, &db, max_distance, &msm);
        for (j, d) in hits.into_iter().filter(|(j, _)| *j != i).take(k) {
            let b = &trajs[j];
            // weight ∈ (0,1]: identical trajectories → 1, distant → →0.
            let weight = 1.0 / (1.0 + d);
            out.push((
                a.node_id.clone(),
                a.node_type.clone(),
                b.node_id.clone(),
                b.node_type.clone(),
                weight,
                d,
            ));
        }
    }
    out
}

fn trajectory_db(trajs: &[Traj]) -> Vec<(usize, Vec<f64>)> {
    trajs
        .iter()
        .enumerate()
        .map(|(idx, traj)| (idx, traj.series.clone()))
        .collect()
}

fn nearest_by_msm_lb(
    probe: &[f64],
    db: &[(usize, Vec<f64>)],
    msm: &MsmConfig,
    k: usize,
    exclude_idx: Option<usize>,
) -> Vec<(usize, f64)> {
    if db.is_empty() || k == 0 {
        return Vec::new();
    }

    let mut threshold = seed_threshold(probe);
    loop {
        let hits = search_with_lb_parallel(probe, db, threshold, msm);
        let mut filtered: Vec<(usize, f64)> = hits
            .into_iter()
            .filter(|(idx, _)| Some(*idx) != exclude_idx)
            .collect();
        if filtered.len() >= k || threshold >= MAX_MSM_THRESHOLD {
            filtered.truncate(k);
            return filtered;
        }
        threshold *= 2.0;
    }
}

fn seed_threshold(probe: &[f64]) -> f64 {
    let scale = probe.iter().fold(0.0_f64, |m, x| m.max(x.abs())).max(1.0);
    scale * (probe.len().max(1) as f64) * 0.1 + 1.0
}

/// One record's categorical event-sequence (Stage 5e).
struct WfTraj {
    node_id: String,
    node_type: String,
    tokens: Vec<String>,
}

/// work_item status-transition sequences (≥ `min_points` transitions).
async fn work_item_status_sequences(
    pool: &PgPool,
    min_points: i64,
) -> Result<Vec<WfTraj>, sqlx::Error> {
    let rows: Vec<(i64, Vec<String>)> = sqlx::query_as(
        "SELECT item_id, array_agg(to_status ORDER BY created_at) AS seq
         FROM work_item_status_history
         GROUP BY item_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, seq)| {
            (seq.len() as i64 >= min_points).then(|| WfTraj {
                node_id: format!("work_item:{id}"),
                node_type: "work_item".to_string(),
                tokens: seq,
            })
        })
        .collect())
}

/// Wagner–Fischer edit distance over any equatable token slice — the base case
/// of the weighted-FST edit distance for categorical event sequences.
fn edit_distance<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// k-NN over categorical sequences by edit distance (Stage 5e workflow_like).
fn workflow_knn(trajs: &[WfTraj], k: usize) -> Vec<(String, String, String, String, f64, f64)> {
    let mut out = Vec::new();
    for (i, a) in trajs.iter().enumerate() {
        let mut dists: Vec<(usize, usize)> = Vec::new();
        for (j, b) in trajs.iter().enumerate() {
            if i == j {
                continue;
            }
            dists.push((j, edit_distance(&a.tokens, &b.tokens)));
        }
        dists.sort_by_key(|x| x.1);
        for (j, d) in dists.into_iter().take(k) {
            let b = &trajs[j];
            let weight = 1.0 / (1.0 + d as f64);
            out.push((
                a.node_id.clone(),
                a.node_type.clone(),
                b.node_id.clone(),
                b.node_type.clone(),
                weight,
                d as f64,
            ));
        }
    }
    out
}

/// Compute trajectory similarities for all sources and replace the
/// `trajectory_similarities` table contents in one transaction.
/// project dependency-accrual trajectories: per-project cumulative count of
/// live+historical `project_depends_on` edges bucketed by the week of each
/// edge's `valid_from` (cross-project coupling growth over time). `evolves_like`
/// then links projects whose dependency-accumulation curves move alike — "which
/// projects are accreting/shedding dependencies in the same shape?".
async fn project_depends_on_trajectories(
    pool: &PgPool,
    min_points: i64,
) -> Result<Vec<Traj>, sqlx::Error> {
    let rows: Vec<(i32, Vec<f64>)> = sqlx::query_as(
        "WITH weekly AS (
            SELECT dependent_project_id AS pid,
                   date_trunc('week', valid_from) AS wk,
                   COUNT(*)::float8 AS added
              FROM project_dependencies
             GROUP BY dependent_project_id, date_trunc('week', valid_from)
         )
         SELECT pid, array_agg(added ORDER BY wk) AS series
           FROM weekly
          GROUP BY pid",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(pid, weekly_adds)| {
            let mut cum = 0.0;
            let series: Vec<f64> = weekly_adds
                .into_iter()
                .map(|a| {
                    cum += a;
                    cum
                })
                .collect();
            (series.len() as i64 >= min_points).then(|| Traj {
                node_id: format!("project:{pid}"),
                node_type: "project_depends_on".to_string(),
                series,
            })
        })
        .collect())
}

pub async fn run_trajectory_similarity(
    pool: &PgPool,
    stats: &StatsTracker,
    config: &TrajectorySimilarityConfig,
) -> Result<(), sqlx::Error> {
    let cap = config.max_per_type.max(1) as usize;
    let kk = config.k_neighbors.max(1) as usize;
    // (from_id, from_type, to_id, to_type, weight, distance, edge_kind)
    let mut edges: Vec<(String, String, String, String, f64, f64, &'static str)> = Vec::new();

    // Stage 5c — numeric MSM `evolves_like` (work_item progress, file churn).
    // Bound O(n²): keep the most-sampled trajectories per type.
    for mut cohort in [
        work_item_trajectories(pool, config.min_points).await?,
        file_trajectories(pool, config.min_points).await?,
        deadlock_risk_trajectories(pool, config.min_points).await?,
        lock_contention_trajectories(pool, config.min_points).await?,
        project_depends_on_trajectories(pool, config.min_points).await?,
    ] {
        cohort.sort_by_key(|t| std::cmp::Reverse(t.series.len()));
        cohort.truncate(cap);
        for (fi, ft, ti, tt, w, d) in msm_knn(&cohort, config.msm_c, config.max_distance, kk) {
            edges.push((fi, ft, ti, tt, w, d, "evolves_like"));
        }
    }

    // Stage 5e — categorical `workflow_like` (work_item status-transition sequences).
    let mut wf = work_item_status_sequences(pool, config.min_points).await?;
    wf.sort_by_key(|t| std::cmp::Reverse(t.tokens.len()));
    wf.truncate(cap);
    for (fi, ft, ti, tt, w, d) in workflow_knn(&wf, kk) {
        edges.push((fi, ft, ti, tt, w, d, "workflow_like"));
    }

    // Replace the materialized edge set atomically.
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM trajectory_similarities")
        .execute(&mut *tx)
        .await?;
    for (from_id, from_type, to_id, to_type, weight, dist, kind) in &edges {
        sqlx::query(
            "INSERT INTO trajectory_similarities
                (from_node_id, from_type, to_node_id, to_type, weight, msm_distance, edge_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (from_node_id, to_node_id, edge_kind) DO UPDATE
               SET weight = EXCLUDED.weight, msm_distance = EXCLUDED.msm_distance,
                   computed_at = NOW()",
        )
        .bind(from_id)
        .bind(from_type)
        .bind(to_id)
        .bind(to_type)
        .bind(weight)
        .bind(dist)
        .bind(*kind)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    stats
        .trajectory_similarity_runs
        .fetch_add(1, Ordering::Relaxed);
    stats
        .trajectory_edges_emitted
        .store(edges.len() as u64, Ordering::Relaxed);

    // Surface the new `evolves_like` edges immediately.
    crate::db::queries::refresh_memory_unified_edges(pool).await?;
    info!(
        evolves_like_edges = edges.len(),
        "trajectory-similarity pass complete"
    );
    Ok(())
}

/// Stage 5d (online recognition): match a **partial / in-progress** trajectory
/// of `node_type` (`"work_item"` | `"file"`) against the complete reference
/// cohort via MSM — which natively aligns sequences of different lengths, so an
/// *unfolding* series can be scored against *complete* references without
/// waiting for it to finish. Returns the `k` nearest references as
/// `(node_id, msm_distance)`. This is the pull-based form of the streaming
/// transducer: fed the live prefix (an RLM run's steps so far, a work-item's
/// progress to date, a file's churn this week), it surfaces the most similar
/// known trajectories for early-warning / outcome prediction.
pub async fn recognize_partial_trajectory(
    pool: &PgPool,
    node_type: &str,
    partial: &[f64],
    k: usize,
    msm_c: f64,
) -> Result<Vec<(String, f64)>, sqlx::Error> {
    if partial.is_empty() {
        return Ok(Vec::new());
    }
    // min_points = 1: every reference is a candidate, however short.
    let cohort = match node_type {
        "work_item" => work_item_trajectories(pool, 1).await?,
        "file" => file_trajectories(pool, 1).await?,
        "deadlock_risk" => deadlock_risk_trajectories(pool, 1).await?,
        "lock_contention" => lock_contention_trajectories(pool, 1).await?,
        _ => return Ok(Vec::new()),
    };
    let msm = MsmConfig::new(if msm_c > 0.0 { msm_c } else { 0.1 });
    let db = trajectory_db(&cohort);
    Ok(nearest_by_msm_lb(partial, &db, &msm, k.max(1), None)
        .into_iter()
        .map(|(idx, distance)| (cohort[idx].node_id.clone(), distance))
        .collect())
}

/// Run the pass, logging any error rather than panicking the cron thread.
pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    config: TrajectorySimilarityConfig,
) {
    if let Err(e) = run_trajectory_similarity(&pool, &stats, &config).await {
        warn!(error = %e, "trajectory-similarity pass failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trajs() -> Vec<Traj> {
        vec![
            Traj {
                node_id: "a".to_string(),
                node_type: "test".to_string(),
                series: vec![1.0, 2.0, 3.0],
            },
            Traj {
                node_id: "b".to_string(),
                node_type: "test".to_string(),
                series: vec![1.0, 2.0, 3.1],
            },
            Traj {
                node_id: "c".to_string(),
                node_type: "test".to_string(),
                series: vec![10.0, 10.0, 10.0],
            },
        ]
    }

    fn brute_knn(trajs: &[Traj], c: f64, max_distance: f64, k: usize) -> Vec<(String, String)> {
        let msm = MsmConfig::new(c);
        let mut out = Vec::new();
        for (i, a) in trajs.iter().enumerate() {
            let mut dists: Vec<(usize, f64)> = trajs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(j, b)| {
                    let d = msm.distance(&a.series, &b.series);
                    (d <= max_distance).then_some((j, d))
                })
                .collect();
            dists.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal));
            out.extend(
                dists
                    .into_iter()
                    .take(k)
                    .map(|(j, _)| (a.node_id.clone(), trajs[j].node_id.clone())),
            );
        }
        out
    }

    #[test]
    fn msm_knn_matches_brute_force_ids() {
        let trajs = sample_trajs();
        let got: Vec<(String, String)> = msm_knn(&trajs, 0.1, 100.0, 2)
            .into_iter()
            .map(|(from_id, _, to_id, _, _, _)| (from_id, to_id))
            .collect();
        assert_eq!(got, brute_knn(&trajs, 0.1, 100.0, 2));
    }

    #[test]
    fn nearest_by_msm_lb_excludes_and_limits() {
        let trajs = sample_trajs();
        let db = trajectory_db(&trajs);
        let msm = MsmConfig::new(0.1);
        let hits = nearest_by_msm_lb(&trajs[0].series, &db, &msm, 1, Some(0));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 1);
    }
}
