//! Async writer for per-MCP-call durable telemetry.
//!
//! Tool calls are timed and recorded into in-memory atomics by
//! `instrumented_tool_wrap` in `src/mcp/server.rs` (Tier 1). This module
//! additionally enqueues a `TelemetryRow` per call onto a bounded mpsc
//! channel drained by `run_telemetry_writer` which batches rows into the
//! `mcp_tool_calls` table.
//!
//! Privacy posture (matches `session_prompts`): the row carries tool name,
//! client name, client version, MCP protocol version, duration, outcome,
//! request id — never the raw params (only an optional `params_sha256`).
//!
//! Backpressure: the channel is bounded at `TELEMETRY_CHANNEL_CAPACITY`.
//! On overflow we drop the row and increment `telemetry_writes_dropped`
//! so observability remains observable.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::MetricsConfig;
use crate::stats::tracker::StatsTracker;

/// Channel capacity for the telemetry writer. Sized so an ~8000 rps
/// burst can be absorbed before drop-on-overflow kicks in; under
/// sustained pressure the `telemetry_sample_rate` config knob can be
/// lowered to reduce volume.
pub const TELEMETRY_CHANNEL_CAPACITY: usize = 4096;

/// One pending durable-telemetry row. Constructed by `instrumented_tool_wrap`
/// and pushed onto the writer channel; flushed in batches by
/// `run_telemetry_writer`. All fields except `outcome`/`tool`/
/// `client_name`/`duration_ms` are nullable in the DB.
#[derive(Clone, Debug)]
pub struct TelemetryRow {
    pub tool: String,
    pub client_name: String,
    pub client_version: Option<String>,
    pub protocol_version: Option<String>,
    pub mcp_session_id: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub duration_ms: i32,
    pub outcome: &'static str, // 'ok' | 'error' | 'timeout' | 'cancelled'
    pub error_class: Option<String>,
    pub request_id: Option<String>,
    pub params_sha256: Option<String>,
}

/// Spawn the writer task. Returns the join handle; the caller stores it
/// for graceful shutdown. The writer reads rows from the channel returned
/// by `start_telemetry_writer` and the sender side is registered on the
/// StatsTracker so `instrumented_tool_wrap` can push without holding a
/// task-local handle.
pub fn start_telemetry_writer(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    config: MetricsConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let (tx, rx) = mpsc::channel::<TelemetryRow>(TELEMETRY_CHANNEL_CAPACITY);
    stats.set_telemetry_sender(tx);

    info!(
        batch_size = config.telemetry_batch_size,
        batch_interval_ms = config.telemetry_batch_interval_ms,
        retention_days = config.telemetry_retention_days,
        sample_rate = config.telemetry_sample_rate,
        "telemetry writer task starting"
    );

    tokio::spawn(run_telemetry_writer(pool, stats, config, rx, cancel))
}

async fn run_telemetry_writer(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    config: MetricsConfig,
    mut rx: mpsc::Receiver<TelemetryRow>,
    cancel: CancellationToken,
) {
    let batch_size = config.telemetry_batch_size.max(1);
    let batch_interval = Duration::from_millis(config.telemetry_batch_interval_ms.max(1));
    let mut buffer: Vec<TelemetryRow> = Vec::with_capacity(batch_size);
    let mut last_flush = Instant::now();

    loop {
        // Compute how long to wait before the next forced flush.
        let elapsed = last_flush.elapsed();
        let until_flush = batch_interval.saturating_sub(elapsed);

        let recv = tokio::select! {
            _ = cancel.cancelled() => {
                debug!(remaining = buffer.len(), "telemetry writer: shutdown requested");
                if !buffer.is_empty() {
                    flush_batch(&pool, &stats, &buffer).await;
                }
                while let Ok(row) = rx.try_recv() {
                    buffer.push(row);
                    if buffer.len() >= batch_size {
                        flush_batch(&pool, &stats, &buffer).await;
                        buffer.clear();
                    }
                }
                if !buffer.is_empty() {
                    flush_batch(&pool, &stats, &buffer).await;
                }
                info!("telemetry writer exited");
                return;
            }
            maybe = tokio::time::timeout(until_flush, rx.recv()) => {
                match maybe {
                    Ok(Some(row)) => Some(row),
                    Ok(None) => {
                        debug!("telemetry channel closed");
                        if !buffer.is_empty() {
                            flush_batch(&pool, &stats, &buffer).await;
                        }
                        return;
                    }
                    Err(_) => None,
                }
            }
        };

        if let Some(row) = recv {
            buffer.push(row);
        }

        let size_trigger = buffer.len() >= batch_size;
        let time_trigger = last_flush.elapsed() >= batch_interval && !buffer.is_empty();
        if size_trigger || time_trigger {
            flush_batch(&pool, &stats, &buffer).await;
            buffer.clear();
            last_flush = Instant::now();
        }
    }
}

async fn flush_batch(pool: &PgPool, stats: &StatsTracker, rows: &[TelemetryRow]) {
    if rows.is_empty() {
        return;
    }
    // Build a single INSERT with UNNEST to amortize round-trip cost.
    let n = rows.len();
    let mut tools = Vec::with_capacity(n);
    let mut client_names = Vec::with_capacity(n);
    let mut client_versions = Vec::with_capacity(n);
    let mut protocol_versions = Vec::with_capacity(n);
    let mut mcp_session_ids = Vec::with_capacity(n);
    let mut projects = Vec::with_capacity(n);
    let mut cwds = Vec::with_capacity(n);
    let mut durations = Vec::with_capacity(n);
    let mut outcomes = Vec::with_capacity(n);
    let mut error_classes = Vec::with_capacity(n);
    let mut request_ids = Vec::with_capacity(n);
    let mut params_hashes = Vec::with_capacity(n);
    for r in rows {
        tools.push(r.tool.clone());
        client_names.push(r.client_name.clone());
        client_versions.push(r.client_version.clone().unwrap_or_default());
        protocol_versions.push(r.protocol_version.clone().unwrap_or_default());
        mcp_session_ids.push(r.mcp_session_id.clone().unwrap_or_default());
        projects.push(r.project.clone().unwrap_or_default());
        cwds.push(r.cwd.clone().unwrap_or_default());
        durations.push(r.duration_ms);
        outcomes.push(r.outcome.to_string());
        error_classes.push(r.error_class.clone().unwrap_or_default());
        request_ids.push(r.request_id.clone().unwrap_or_default());
        params_hashes.push(r.params_sha256.clone().unwrap_or_default());
    }

    let sql = "INSERT INTO mcp_tool_calls
        (tool, client_name, client_version, protocol_version,
         mcp_session_id, project, cwd, duration_ms, outcome,
         error_class, request_id, params_sha256)
        SELECT *
        FROM UNNEST(
            $1::text[],
            $2::text[],
            NULLIF($3::text[], ARRAY[]::text[]),
            NULLIF($4::text[], ARRAY[]::text[]),
            NULLIF($5::text[], ARRAY[]::text[]),
            NULLIF($6::text[], ARRAY[]::text[]),
            NULLIF($7::text[], ARRAY[]::text[]),
            $8::int[],
            $9::text[],
            NULLIF($10::text[], ARRAY[]::text[]),
            NULLIF($11::text[], ARRAY[]::text[]),
            NULLIF($12::text[], ARRAY[]::text[])
        )";

    let result = sqlx::query(sql)
        .bind(&tools)
        .bind(&client_names)
        .bind(&client_versions)
        .bind(&protocol_versions)
        .bind(&mcp_session_ids)
        .bind(&projects)
        .bind(&cwds)
        .bind(&durations)
        .bind(&outcomes)
        .bind(&error_classes)
        .bind(&request_ids)
        .bind(&params_hashes)
        .execute(pool)
        .await;
    match result {
        Ok(r) => {
            stats
                .telemetry_rows_written
                .fetch_add(r.rows_affected(), Ordering::Relaxed);
            debug!(rows = r.rows_affected(), "telemetry batch flushed");
        }
        Err(e) => {
            warn!(rows = n, error = %e, "telemetry batch flush failed");
            stats
                .telemetry_writes_failed
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

/// Try to enqueue a telemetry row from `instrumented_tool_wrap`. Returns
/// `true` if accepted, `false` if dropped (channel full or no sender
/// registered, e.g. CLI mode). The dropped case increments
/// `telemetry_writes_dropped` on the StatsTracker.
pub fn try_enqueue(stats: &StatsTracker, row: TelemetryRow) -> bool {
    let Some(tx) = stats.telemetry_sender() else {
        return false; // No writer registered (CLI mode or telemetry disabled).
    };
    match tx.try_send(row) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            stats.telemetry_writes_dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            error!("telemetry channel closed unexpectedly");
            false
        }
    }
}
