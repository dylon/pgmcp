//! MCP-client liveness sweep cron.
//!
//! Periodic sweep over `mcp_clients` rows still marked `alive`: re-checks each
//! resolved PID via `/proc` (existence **and** a start-time fingerprint, so a
//! recycled PID is treated as exited rather than a false "still alive"),
//! refreshes cwd + resolved project for the survivors, and flips dead clients to
//! `alive=false, exited_at=now()`. Also prunes the in-memory capture-dedup set
//! (`StatsTracker::remove_seen_client`) so it does not grow without bound.
//!
//! When `[clients] proc_fd_supplement` is on, the sweep additionally samples
//! each live client's `/proc/<pid>/fd` and records currently-open files (that
//! belong to an indexed project) as `proc_fd` `client_file_events`. This is the
//! best-effort, low-signal Phase-2 supplement — near-blind to open-close editors
//! like Claude Code, so it is off by default. Light job otherwise (a bounded
//! SELECT + per-row UPDATEs); modeled on `work_item_presence`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::stats::tracker::StatsTracker;

#[derive(sqlx::FromRow)]
struct AliveClient {
    mcp_session_id: String,
    pid: Option<i32>,
    proc_start_ticks: Option<i64>,
    #[allow(dead_code)]
    project_id: Option<i32>,
}

/// One liveness sweep. `pool` is an owned `PgPool` (cheaply cloned from
/// `DbClient::pool()`). `proc_fd` enables the open-files supplement.
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>, proc_fd: bool) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match sweep(&pool, &stats, proc_fd).await {
        Ok((checked, exited, refreshed)) => {
            if exited + refreshed > 0 {
                info!(
                    checked,
                    exited, refreshed, proc_fd, "mcp-client-liveness cron: swept clients"
                );
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "mcp-client-liveness cron: sweep failed");
        }
    }
}

async fn sweep(
    pool: &PgPool,
    stats: &StatsTracker,
    proc_fd: bool,
) -> Result<(usize, usize, usize), sqlx::Error> {
    let rows: Vec<AliveClient> = sqlx::query_as::<_, AliveClient>(
        "SELECT mcp_session_id, pid, proc_start_ticks, project_id FROM mcp_clients WHERE alive",
    )
    .fetch_all(pool)
    .await?;

    let checked = rows.len();
    let mut exited = 0usize;
    let mut refreshed = 0usize;

    for r in rows {
        let Some(pid) = r.pid else {
            continue;
        };

        // PID existence + reuse guard + cwd read (+ open files when sampling)
        // run off the async executor.
        let expected_ticks = r.proc_start_ticks;
        let (alive, cwd, open_files): (bool, Option<String>, Vec<PathBuf>) =
            tokio::task::spawn_blocking(move || {
                if !crate::proc_clients::pid_alive(pid) {
                    return (false, None, Vec::new());
                }
                if let Some(expected) = expected_ticks {
                    match crate::proc_clients::proc_start_ticks(pid) {
                        Some(now) if now as i64 == expected => {}
                        _ => return (false, None, Vec::new()), // recycled/unreadable → exited
                    }
                }
                let cwd = crate::proc_clients::read_process_cwd(pid)
                    .map(|c| c.to_string_lossy().into_owned());
                let files = if proc_fd {
                    crate::proc_clients::list_open_files(pid)
                } else {
                    Vec::new()
                };
                (true, cwd, files)
            })
            .await
            .unwrap_or((true, None, Vec::new())); // join error → leave alive

        if alive {
            let project_id = match &cwd {
                Some(c) => crate::db::queries::find_project_by_cwd(pool, c)
                    .await?
                    .map(|p| p.id),
                None => None,
            };
            sqlx::query(
                "UPDATE mcp_clients
                    SET last_liveness_at = now(),
                        cwd        = COALESCE($2, cwd),
                        project_id = COALESCE($3, project_id)
                  WHERE mcp_session_id = $1 AND alive",
            )
            .bind(&r.mcp_session_id)
            .bind(&cwd)
            .bind(project_id)
            .execute(pool)
            .await?;
            refreshed += 1;

            if proc_fd && !open_files.is_empty() {
                record_open_files(pool, &r.mcp_session_id, pid, &open_files).await?;
            }
        } else {
            sqlx::query(
                "UPDATE mcp_clients
                    SET alive = FALSE, exited_at = now(), last_liveness_at = now()
                  WHERE mcp_session_id = $1 AND alive",
            )
            .bind(&r.mcp_session_id)
            .execute(pool)
            .await?;
            stats.remove_seen_client(&r.mcp_session_id);
            exited += 1;
        }
    }

    Ok((checked, exited, refreshed))
}

/// Record `proc_fd` file events for a client's currently-open files, each
/// resolved to its own project + indexed file. Files that do not fall under any
/// indexed project (editor transcripts, logs, system libs) are skipped, so the
/// supplement stays scoped to real working files.
async fn record_open_files(
    pool: &PgPool,
    mcp_session_id: &str,
    pid: i32,
    files: &[PathBuf],
) -> Result<(), sqlx::Error> {
    for path in files {
        let path = path.to_string_lossy();
        let Some(project_id) = crate::db::queries::find_project_by_cwd(pool, &path)
            .await?
            .map(|p| p.id)
        else {
            continue; // not under any indexed project
        };
        let file_id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM indexed_files WHERE path = $1")
                .bind(path.as_ref())
                .fetch_optional(pool)
                .await?;
        sqlx::query(
            "INSERT INTO client_file_events
                (mcp_session_id, pid, file_id, project_id, abs_path, op, source, ts)
             VALUES ($1, $2, $3, $4, $5, 'open', 'proc_fd', now())",
        )
        .bind(mcp_session_id)
        .bind(pid)
        .bind(file_id)
        .bind(project_id)
        .bind(path.as_ref())
        .execute(pool)
        .await?;
    }
    Ok(())
}
