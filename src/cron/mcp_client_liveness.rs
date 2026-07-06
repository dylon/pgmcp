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
use tracing::{error, info};

use crate::stats::tracker::StatsTracker;

#[derive(sqlx::FromRow)]
struct AliveClient {
    mcp_session_id: String,
    pid: Option<i32>,
    proc_start_ticks: Option<i64>,
    /// Last-known project (carried into the realtime disconnect event).
    project_id: Option<i32>,
}

/// One liveness sweep. `pool` is an owned `PgPool` (cheaply cloned from
/// `DbClient::pool()`). `proc_fd` enables the open-files supplement.
pub async fn run_or_log(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    proc_fd: bool,
    stale_after_secs: u64,
) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match sweep(&pool, &stats, proc_fd, stale_after_secs).await {
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
            error!(error = %e, "mcp-client-liveness cron: sweep failed");
        }
    }
}

async fn sweep(
    pool: &PgPool,
    stats: &StatsTracker,
    proc_fd: bool,
    stale_after_secs: u64,
) -> Result<(usize, usize, usize), sqlx::Error> {
    // Time backstop: expire NULL-pid `alive` rows with no recent activity. The
    // capture writer records `pid=NULL` when the TCP-peer→PID resolution fails;
    // such a row has no `/proc` handle to liveness-check, so without this it
    // would stay `alive` forever (surviving reboots — there is no stale PID to
    // invalidate). Keyed on activity time, so it is reboot-safe. Has-pid rows
    // are handled by the per-row `/proc` check below.
    let time_expired = sqlx::query(
        "UPDATE mcp_clients
            SET alive = FALSE, exited_at = now(), cgroup_id = NULL
          WHERE alive
            AND pid IS NULL
            AND COALESCE(last_liveness_at, last_seen) < now() - make_interval(secs => $1)",
    )
    .bind(stale_after_secs as f64)
    .execute(pool)
    .await?
    .rows_affected() as usize;

    let rows: Vec<AliveClient> = sqlx::query_as::<_, AliveClient>(
        "SELECT mcp_session_id, pid, proc_start_ticks, project_id FROM mcp_clients WHERE alive",
    )
    .fetch_all(pool)
    .await?;

    let checked = rows.len();
    let mut exited = time_expired;
    let mut refreshed = 0usize;

    for r in rows {
        let Some(pid) = r.pid else {
            continue;
        };

        // PID existence + reuse guard + cwd read (+ open files when sampling)
        // run off the async executor.
        let expected_ticks = r.proc_start_ticks;
        let (alive, cwd, open_files, cgroup_id): (bool, Option<String>, Vec<PathBuf>, Option<u64>) =
            tokio::task::spawn_blocking(move || {
                if !crate::proc_clients::pid_alive(pid) {
                    return (false, None, Vec::new(), None);
                }
                if let Some(expected) = expected_ticks {
                    match crate::proc_clients::proc_start_ticks(pid) {
                        Some(now) if now as i64 == expected => {}
                        _ => return (false, None, Vec::new(), None), // recycled/unreadable → exited
                    }
                }
                let cwd = crate::proc_clients::read_process_cwd(pid)
                    .map(|c| c.to_string_lossy().into_owned());
                let files = if proc_fd {
                    crate::proc_clients::list_open_files(pid)
                } else {
                    Vec::new()
                };
                // Refresh the cgroup id (a process can change cgroups mid-life,
                // though agent subtrees rarely do); the eBPF probe filters on it.
                let cgroup = crate::proc_clients::read_process_cgroup_id(pid);
                (true, cwd, files, cgroup)
            })
            .await
            .unwrap_or((true, None, Vec::new(), None)); // join error → leave alive

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
                        project_id = COALESCE($3, project_id),
                        cgroup_id  = COALESCE($4, cgroup_id)
                  WHERE mcp_session_id = $1 AND alive",
            )
            .bind(&r.mcp_session_id)
            .bind(&cwd)
            .bind(project_id)
            .bind(cgroup_id.map(|c| c as i64))
            .execute(pool)
            .await?;
            refreshed += 1;

            if proc_fd && !open_files.is_empty() {
                // Emit into the reactive ingestion stream (ADR-022); the batched
                // writer resolves project/indexed-file and inserts. Synchronous,
                // best-effort (drops on a full buffer).
                record_open_files(stats, &r.mcp_session_id, pid, &open_files);
            }
        } else {
            sqlx::query(
                // Clear cgroup_id on exit so a recycled cgroup inode can't
                // mis-attribute a later subprocess event to this dead client.
                "UPDATE mcp_clients
                    SET alive = FALSE, exited_at = now(), last_liveness_at = now(),
                        cgroup_id = NULL
                  WHERE mcp_session_id = $1 AND alive",
            )
            .bind(&r.mcp_session_id)
            .execute(pool)
            .await?;
            // Realtime event (topic=client): the client exited. Own-tx,
            // best-effort — the sweep must not fail on a telemetry write.
            crate::realtime::emit(
                pool,
                &crate::realtime::RealtimeEvent::client_disconnect(&r.mcp_session_id, r.project_id),
            )
            .await;
            stats.remove_seen_client(&r.mcp_session_id);
            exited += 1;
        }
    }

    Ok((checked, exited, refreshed))
}

/// Emit `proc_fd` file-touch events for a client's currently-open files into the
/// reactive ingestion stream (ADR-022). Each open file is stamped with the owning
/// `mcp_session_id` + PID as a [`FileOp::Open`]; the batched writer resolves the
/// project + indexed file (skipping nothing here — a file under a workspace root
/// but outside any project simply records a NULL project_id) and performs the
/// insert. Synchronous and best-effort: `emit_file_event` drops on a full buffer,
/// so the liveness tick never blocks on the supplement.
fn record_open_files(stats: &StatsTracker, mcp_session_id: &str, pid: i32, files: &[PathBuf]) {
    use crate::proc_clients::file_events::{FileEventSource, FileOp, FileTouchEvent};
    for path in files {
        stats.emit_file_event(FileTouchEvent {
            source: FileEventSource::ProcFd,
            op: FileOp::Open,
            abs_path: path.to_string_lossy().into_owned(),
            pid: Some(pid),
            ppid: None,
            root_pid: None,
            cgroup_id: None,
            mcp_session_id: Some(mcp_session_id.to_string()),
            session_id: None,
            agent_id: None,
        });
    }
}
