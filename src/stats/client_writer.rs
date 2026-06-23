//! Async writer for MCP-client OS-identity capture.
//!
//! On the first tool call of a session, `instrumented_tool_wrap`
//! (`src/mcp/server.rs`) calls [`StatsTracker::note_client`], which — once per
//! session, deduped by an in-memory set — enqueues a [`ClientObservation`] onto
//! a bounded channel drained here. The writer resolves the client's OS identity
//! from `/proc` OFF the hot path (in `spawn_blocking`: PID from the TCP peer,
//! cwd, start-ticks), maps cwd → project, and upserts `mcp_clients`. Keeping the
//! ~100 ms `/proc` scan out of the tool-call future means per-call capture
//! overhead is just a `DashMap` dedup check plus a non-blocking channel send.
//!
//! Privacy posture matches `mcp_tool_calls`/`session_prompts`: the row carries
//! client name/version, protocol, PID, cwd, and resolved project — all local,
//! never shipped remotely.

use std::net::SocketAddr;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::stats::tracker::StatsTracker;

/// Channel capacity. Client captures are once-per-session (low volume), so a
/// small buffer suffices; on overflow we drop (the capture is retried never,
/// but the liveness cron's periodic reconcile still backfills the row).
pub const CLIENT_CHANNEL_CAPACITY: usize = 256;

/// One client-identity capture request, enqueued once per session by
/// `note_client` and resolved+upserted by `run_client_writer`.
#[derive(Clone, Debug)]
pub struct ClientObservation {
    pub mcp_session_id: String,
    pub client_name: String,
    pub client_version: Option<String>,
    pub protocol_version: Option<String>,
    /// The client's TCP source address as seen by the daemon (the accepted
    /// connection's *remote* address).
    pub peer: SocketAddr,
    /// The daemon's MCP listen port (the connection's *remote* port from the
    /// client's perspective) — the `/proc/net/tcp` disambiguator.
    pub server_port: u16,
}

/// Spawn the client-capture writer task and register its sender on the tracker.
/// Mirrors `start_telemetry_writer`.
pub fn start_client_writer(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let (tx, rx) = mpsc::channel::<ClientObservation>(CLIENT_CHANNEL_CAPACITY);
    stats.set_client_sender(tx);
    info!("mcp-client capture writer task starting");
    tokio::spawn(run_client_writer(pool, rx, cancel))
}

async fn run_client_writer(
    pool: PgPool,
    mut rx: mpsc::Receiver<ClientObservation>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("mcp-client capture writer: shutdown requested");
                return;
            }
            maybe = rx.recv() => {
                match maybe {
                    Some(obs) => {
                        if let Err(e) = resolve_and_upsert(&pool, &obs).await {
                            error!(
                                session = %obs.mcp_session_id,
                                error = %e,
                                "mcp-client capture upsert failed"
                            );
                        }
                    }
                    None => {
                        debug!("mcp-client capture channel closed");
                        return;
                    }
                }
            }
        }
    }
}

/// Resolve the client's PID/cwd/start-ticks from `/proc` (blocking syscalls run
/// in `spawn_blocking`), map cwd → project, and upsert `mcp_clients`.
async fn resolve_and_upsert(pool: &PgPool, obs: &ClientObservation) -> Result<(), sqlx::Error> {
    let peer = obs.peer;
    let server_port = obs.server_port;
    let (pid, cwd, start_ticks, cgroup_id): (
        Option<i32>,
        Option<String>,
        Option<u64>,
        Option<u64>,
    ) = tokio::task::spawn_blocking(move || {
        let pid = crate::proc_clients::resolve_pid_for_peer(peer, server_port);
        match pid {
            Some(p) => (
                Some(p),
                crate::proc_clients::read_process_cwd(p).map(|c| c.to_string_lossy().into_owned()),
                crate::proc_clients::proc_start_ticks(p),
                // cgroup-v2 id of the client process — the eBPF subtree-capture
                // probe (ADR-022) filters by this, so resolving it at first sight
                // means a client's subprocesses are traceable without waiting for
                // the first liveness tick.
                crate::proc_clients::read_process_cgroup_id(p),
            ),
            None => (None, None, None, None),
        }
    })
    .await
    .unwrap_or((None, None, None, None));

    let project_id = match &cwd {
        Some(c) => crate::db::queries::find_project_by_cwd(pool, c)
            .await?
            .map(|p| p.id),
        None => None,
    };

    sqlx::query(
        "INSERT INTO mcp_clients
            (mcp_session_id, client_name, client_version, protocol_version,
             pid, proc_start_ticks, cwd, project_id, cgroup_id,
             first_seen, last_seen, last_liveness_at, alive)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now(), now(), TRUE)
         ON CONFLICT (mcp_session_id) DO UPDATE SET
            client_name      = EXCLUDED.client_name,
            client_version   = EXCLUDED.client_version,
            protocol_version = EXCLUDED.protocol_version,
            pid              = EXCLUDED.pid,
            proc_start_ticks = EXCLUDED.proc_start_ticks,
            cwd              = EXCLUDED.cwd,
            project_id       = EXCLUDED.project_id,
            cgroup_id        = EXCLUDED.cgroup_id,
            last_seen        = now(),
            last_liveness_at = now(),
            alive            = TRUE,
            exited_at        = NULL",
    )
    .bind(&obs.mcp_session_id)
    .bind(&obs.client_name)
    .bind(&obs.client_version)
    .bind(&obs.protocol_version)
    .bind(pid)
    .bind(start_ticks.map(|t| t as i64))
    .bind(&cwd)
    .bind(project_id)
    .bind(cgroup_id.map(|c| c as i64))
    .execute(pool)
    .await?;

    Ok(())
}
