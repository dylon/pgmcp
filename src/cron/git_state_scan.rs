//! Git-state scan cron + coordination gatekeeper.
//!
//! For every project that is the *dependency* of an open coordination request,
//! it reads the live git state, updates the project's git-state columns, and —
//! when the dependency becomes **stable** (on its stable branch & clean) — posts
//! a `stable_restored` `project_event`, RESOLVES the open coordination requests
//! against it (the only path to `resolved`; see the TLA⁺/Rocq gatekeeper proof),
//! and notifies each requester via the mailbox that it is unblocked.
//!
//! Scoped to projects under active coordination, so it is cheap and responsive
//! even though git reads shell out. Read-only git.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::stats::tracker::StatsTracker;

pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match scan(&pool).await {
        Ok(resolved) if resolved > 0 => {
            info!(
                resolved,
                "git-state-scan cron: resolved coordination requests"
            )
        }
        Ok(_) => {}
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "git-state-scan cron: failed");
        }
    }
}

async fn scan(pool: &PgPool) -> Result<usize, sqlx::Error> {
    // Dependency projects with ≥1 open coordination request.
    let targets: Vec<(i32, String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT p.id, p.path, p.stable_branch
           FROM coordination_requests cr
           JOIN projects p ON p.id = cr.dependency_project_id
          WHERE cr.status IN ('pending', 'accepted', 'moved')",
    )
    .fetch_all(pool)
    .await?;

    let mut resolved_total = 0usize;
    for (pid, path, stable_branch) in targets {
        let state =
            tokio::task::spawn_blocking(move || crate::deps::gitstate::read_git_state(&path))
                .await
                .unwrap_or_default();
        let stable = crate::deps::gitstate::is_stable(&state, stable_branch.as_deref());

        // Refresh the live git-state columns.
        let _ = sqlx::query(
            "UPDATE projects
                SET git_current_branch = $2, git_head_sha = $3,
                    git_dirty = $4, git_scanned_at = now()
              WHERE id = $1",
        )
        .bind(pid)
        .bind(&state.current_branch)
        .bind(&state.head_sha)
        .bind(state.dirty)
        .execute(pool)
        .await;

        if stable {
            // GATEKEEPER: post the event, then resolve + notify. Only this path
            // reaches `resolved` (the trust boundary).
            let _ = sqlx::query(
                "INSERT INTO project_events (project_id, kind) VALUES ($1, 'stable_restored')",
            )
            .bind(pid)
            .execute(pool)
            .await;
            let resolved = crate::deps::coord_store::resolve_and_notify(pool, pid).await?;
            resolved_total += resolved.len();
        }
    }
    Ok(resolved_total)
}
