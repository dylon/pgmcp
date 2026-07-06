//! `experiment-project-backfill` cron: fill `experiments.project_id` for
//! experiments that were opened without one, by inferring the owning project
//! from the provenance the experiment already carries.
//!
//! Inference order (first hit wins), mirroring how an experiment is anchored:
//!   1. `git_ref` — the commit the experiment was opened at → the project that
//!      owns that commit (`git_commits.commit_hash`). Skipped when the commit
//!      hash appears under more than one project (worktree clones of the same
//!      repo), since the assignment would be a coin flip.
//!   2. `plan_ref` — the driving plan's work-item `public_id` → that work item's
//!      project.
//!   3. a linked `work_item_experiment` bridge row → its work item's project.
//!
//! IDEMPOTENT + NON-DESTRUCTIVE: the UPDATE is guarded `WHERE project_id IS
//! NULL`, so a run never overwrites a project already set (by the operator PATCH,
//! creation-time inference, or a prior backfill) and re-running is a no-op once
//! every experiment is anchored. Provenance is logged at `info!` with a count.
//!
//! Interval-gated (`[cron] experiment_project_backfill_interval_secs`, default
//! 6h; 0 disables). Light job (bounded, capped per run) — runs on the runtime
//! like the findings-promotion sweep, no heavy-cron gate.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::stats::tracker::StatsTracker;

/// Max experiments assigned per run — a guardrail so a first sweep over a large
/// backlog does not hold the pool for one long pass (the rest converge on
/// subsequent ticks, since assigned rows drop out of the `project_id IS NULL`
/// filter).
const MAX_BACKFILL_PER_RUN: i64 = 500;

/// A minimum `git_ref` length before it is treated as a (possibly abbreviated)
/// commit hash for prefix matching — avoids an over-broad `LIKE` on a stray
/// short ref. 7 is the conventional Git short-hash length.
const MIN_GIT_REF_LEN: usize = 7;

/// One backfill sweep. `pool` is an owned `PgPool` (cheaply cloned from
/// `DbClient::pool()`). Best-effort: one experiment's inference failure never
/// aborts the sweep.
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);

    // Current (`valid_to IS NULL`) experiments still missing a project.
    let rows: Vec<(i64, Option<String>, Option<String>)> = match sqlx::query_as::<
        _,
        (i64, Option<String>, Option<String>),
    >(
        "SELECT id, git_ref, plan_ref
         FROM experiments
         WHERE project_id IS NULL AND valid_to IS NULL
         ORDER BY id ASC
         LIMIT $1",
    )
    .bind(MAX_BACKFILL_PER_RUN)
    .fetch_all(&pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "experiment-project-backfill: list unassigned experiments failed");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }

    let mut assigned = 0u64;
    for (experiment_id, git_ref, plan_ref) in &rows {
        match infer_project(
            &pool,
            *experiment_id,
            git_ref.as_deref(),
            plan_ref.as_deref(),
        )
        .await
        {
            Ok(Some(project_id)) => match assign_project(&pool, *experiment_id, project_id).await {
                Ok(true) => assigned += 1,
                Ok(false) => {} // raced to non-null by another writer — fine
                Err(e) => error!(
                    error = %e,
                    experiment_id,
                    "experiment-project-backfill: assign failed (non-fatal)"
                ),
            },
            Ok(None) => {} // no signal yet — leave workspace-general
            Err(e) => error!(
                error = %e,
                experiment_id,
                "experiment-project-backfill: inference failed (non-fatal)"
            ),
        }
    }

    if assigned > 0 {
        info!(
            assigned,
            scanned = rows.len(),
            "experiment-project-backfill: assigned project to previously-unowned experiments"
        );
    }
}

/// Infer a project for one experiment via git_ref → plan_ref → bridge.
async fn infer_project(
    pool: &PgPool,
    experiment_id: i64,
    git_ref: Option<&str>,
    plan_ref: Option<&str>,
) -> Result<Option<i32>, sqlx::Error> {
    if let Some(id) = infer_from_git_ref(pool, git_ref).await? {
        return Ok(Some(id));
    }
    if let Some(id) = infer_from_plan_ref(pool, plan_ref).await? {
        return Ok(Some(id));
    }
    infer_from_bridge(pool, experiment_id).await
}

/// The project owning the `git_ref` commit. Requires an UNAMBIGUOUS owner: a
/// commit hash shared by multiple projects (worktree clones) yields `None`
/// rather than an arbitrary pick.
async fn infer_from_git_ref(
    pool: &PgPool,
    git_ref: Option<&str>,
) -> Result<Option<i32>, sqlx::Error> {
    let Some(git_ref) = git_ref
        .map(str::trim)
        .filter(|s| s.len() >= MIN_GIT_REF_LEN)
    else {
        return Ok(None);
    };
    // `commit_hash` is hex, so `LIKE $1 || '%'` (with a hex `$1`) is a safe
    // prefix match with no wildcard hazard; `DISTINCT` + a 2-row probe detects
    // the ambiguous (multi-project) case.
    let owners: Vec<i32> = sqlx::query_scalar::<_, i32>(
        "SELECT DISTINCT project_id
         FROM git_commits
         WHERE project_id IS NOT NULL
           AND (commit_hash = $1 OR commit_hash LIKE $1 || '%')
         LIMIT 2",
    )
    .bind(git_ref)
    .fetch_all(pool)
    .await?;
    match owners.as_slice() {
        [only] => Ok(Some(*only)),
        _ => Ok(None), // zero matches, or ambiguous across projects
    }
}

/// The project of the plan work item named by `plan_ref` (a work-item
/// `public_id`).
async fn infer_from_plan_ref(
    pool: &PgPool,
    plan_ref: Option<&str>,
) -> Result<Option<i32>, sqlx::Error> {
    let Some(plan_ref) = plan_ref.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    sqlx::query_scalar::<_, i32>(
        "SELECT project_id FROM work_items WHERE public_id = $1 AND project_id IS NOT NULL LIMIT 1",
    )
    .bind(plan_ref)
    .fetch_optional(pool)
    .await
}

/// The project of any work item linked to this experiment via the
/// `work_item_experiment` bridge.
async fn infer_from_bridge(pool: &PgPool, experiment_id: i64) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, i32>(
        "SELECT wi.project_id
         FROM work_item_experiment wie
         JOIN work_items wi ON wi.id = wie.work_item_id
         WHERE wie.experiment_id = $1 AND wi.project_id IS NOT NULL
         ORDER BY wie.created_at ASC
         LIMIT 1",
    )
    .bind(experiment_id)
    .fetch_optional(pool)
    .await
}

/// Assign `project_id`, guarded `WHERE project_id IS NULL` so a concurrent write
/// (operator PATCH, creation-time inference) is never clobbered. Returns whether
/// a row was actually updated.
async fn assign_project(
    pool: &PgPool,
    experiment_id: i64,
    project_id: i32,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "UPDATE experiments SET project_id = $2, updated_at = NOW()
         WHERE id = $1 AND project_id IS NULL",
    )
    .bind(experiment_id)
    .bind(project_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}
