//! Durable cron-run ledger (`cron_run_history`, v40).
//!
//! The in-process scheduler (`src/cron/scheduler.rs`) is 100% in-memory: a
//! restart loses every timer, and `record_cron_outcome` keeps only the *latest*
//! outcome per job in a `DashMap`. This module persists **every** run — its
//! intrinsics (duration, RSS delta, thread delta, job counters) and any failure
//! (internal error / panic / gate skip) — so that (a) on startup the scheduler
//! can compute each job's next-due time from its last persisted success
//! (restart-survival, see `restart_initial_delay_ms` in the scheduler), and
//! (b) operators can query history via the `cron_history` MCP tool.
//!
//! Design mirrors the `mcp_tool_calls` telemetry writer
//! (`src/stats/telemetry_writer.rs`): a bounded tokio `mpsc` channel drained by
//! one tokio task that batch-INSERTs via `UNNEST`, with `try_send`
//! drop-on-overflow (counted by `StatsTracker::cron_history_writes_dropped`) so
//! the scheduler / work-pool threads never block. [`CronRunGuard`] is the RAII
//! recorder constructed at the top of each cron body (mirrors `HeavyCronFlag`);
//! its `Drop` writes exactly one row, defaulting to `Panicked` if the body
//! unwound without calling a finisher.

pub mod vocab;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::stats::tracker::{SkipReason, StatsTracker};

pub use vocab::{CronOutcome, CronTriggerSource};

/// Channel capacity. Cron runs are low-volume (a handful per minute steady
/// state; a small burst when in-flight guards drop at shutdown), so 1024 is
/// ample headroom before drop-on-overflow.
pub const CRON_HISTORY_CHANNEL_CAPACITY: usize = 1024;

/// Max rows coalesced into one INSERT.
const MAX_BATCH: usize = 256;

/// One pending `cron_run_history` row. Built by [`CronRunGuard::drop`] or
/// [`CronHistoryWriter::record_skip`] and flushed in batches by the writer task.
#[derive(Clone, Debug)]
pub struct CronRunRecord {
    pub job_name: String,
    pub trigger_source: CronTriggerSource,
    pub outcome: CronOutcome,
    pub skip_reason: Option<SkipReason>,
    pub error_detail: Option<String>,
    pub project: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub rss_mb_start: Option<i64>,
    pub rss_mb_end: Option<i64>,
    pub rss_mb_delta: Option<i64>,
    pub threads_start: Option<i32>,
    pub threads_end: Option<i32>,
    pub threads_delta: Option<i32>,
    pub counters: Value,
}

/// Cloneable handle that rides on `SystemContext`. Holds the writer channel
/// (None in CLI / test mode) plus the `StatsTracker` for the drop counter and
/// the live-RSS gauge. Cheap to clone (one `Option<Sender>` + one `Arc`).
#[derive(Clone)]
pub struct CronHistoryWriter {
    tx: Option<mpsc::Sender<CronRunRecord>>,
    stats: Arc<StatsTracker>,
}

impl CronHistoryWriter {
    /// A no-op writer (no channel). Used by `SystemContext::production` (the
    /// non-daemon/test constructor) and the CLI. Records are silently dropped;
    /// the drop counter is **not** bumped (there is no writer to overflow).
    pub fn null(stats: Arc<StatsTracker>) -> Self {
        Self { tx: None, stats }
    }

    /// Enqueue a record. Never blocks: on a full channel the row is dropped and
    /// `cron_history_writes_dropped` is incremented. No-op when `tx` is `None`.
    pub fn record(&self, rec: CronRunRecord) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(rec) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.stats
                    .cron_history_writes_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task gone (post-shutdown). Nothing to observe.
            }
        }
    }

    /// Record a gate skip directly (no body ran), without the intrinsic capture
    /// of a full [`CronRunGuard`]. Used by `heavy_gate_or_skip!` and the
    /// outer-closure cooldown path.
    pub fn record_skip(
        &self,
        job_name: &str,
        trigger_source: CronTriggerSource,
        reason: SkipReason,
    ) {
        let now = Utc::now();
        self.record(CronRunRecord {
            job_name: job_name.to_string(),
            trigger_source,
            outcome: CronOutcome::Skipped,
            skip_reason: Some(reason),
            error_detail: None,
            project: None,
            started_at: now,
            completed_at: now,
            duration_ms: 0,
            rss_mb_start: None,
            rss_mb_end: None,
            rss_mb_delta: None,
            threads_start: None,
            threads_end: None,
            threads_delta: None,
            counters: Value::Object(Default::default()),
        });
    }
}

/// Exact live RSS in MiB (`/proc/self/statm` via `crate::stats::rss`). Matches
/// the reading the heavy-cron bodies already log, and works without the 500 ms
/// sampler (so CLI-mode guards capture real values too). `None` on read failure
/// (non-Linux). One cheap syscall per guard endpoint — negligible off the hot
/// path (crons run at minute-to-hour cadence).
fn current_rss_mb() -> Option<i64> {
    crate::stats::rss::current_rss_bytes().map(|b| (b >> 20) as i64)
}

/// Live thread count of this process (`/proc/self/task` entry count). `None`
/// off Linux / on read failure.
fn proc_self_thread_count() -> Option<i32> {
    std::fs::read_dir("/proc/self/task")
        .ok()
        .map(|rd| rd.count() as i32)
}

/// RAII recorder for one cron run. Construct at the top of a cron body (mirrors
/// `HeavyCronFlag`); call exactly one finisher (`ok`/`ok_with`/`noop`/`fail`/
/// `skipped`) on the success/error branches. `Drop` computes the end-intrinsics
/// and writes one row; if no finisher ran (the body panicked and unwound through
/// the guard) it records `Panicked`.
pub struct CronRunGuard {
    writer: CronHistoryWriter,
    job_name: String,
    trigger_source: CronTriggerSource,
    project: Option<String>,
    started_at: DateTime<Utc>,
    start_instant: Instant,
    rss_mb_start: Option<i64>,
    threads_start: Option<i32>,
    // Mutated by finishers; defaults record a panic if Drop runs first.
    outcome: CronOutcome,
    skip_reason: Option<SkipReason>,
    error_detail: Option<String>,
    counters: Value,
}

impl CronRunGuard {
    pub fn new(
        writer: CronHistoryWriter,
        job_name: impl Into<String>,
        trigger_source: CronTriggerSource,
        project: Option<String>,
    ) -> Self {
        let rss_mb_start = current_rss_mb();
        Self {
            writer,
            job_name: job_name.into(),
            trigger_source,
            project,
            started_at: Utc::now(),
            start_instant: Instant::now(),
            rss_mb_start,
            threads_start: proc_self_thread_count(),
            outcome: CronOutcome::Panicked, // overwritten by any finisher
            skip_reason: None,
            error_detail: None,
            counters: Value::Object(Default::default()),
        }
    }

    /// Body completed normally.
    pub fn ok(&mut self) {
        self.outcome = CronOutcome::Ok;
    }

    /// Body completed normally with job-specific counters (e.g.
    /// `{"topics_discovered": 42}`).
    pub fn ok_with(&mut self, counters: Value) {
        self.outcome = CronOutcome::Ok;
        self.counters = counters;
    }

    /// Body entered but the empty-data path returned immediately.
    pub fn noop(&mut self) {
        self.outcome = CronOutcome::NoOp;
    }

    /// Body returned an internal error from its top-level `Err` arm.
    pub fn fail(&mut self, detail: impl Into<String>) {
        self.outcome = CronOutcome::Failed;
        self.error_detail = Some(detail.into());
    }

    /// Body short-circuited at a gate (rare; the gate path normally uses
    /// [`CronHistoryWriter::record_skip`]).
    #[allow(dead_code)]
    pub fn skipped(&mut self, reason: SkipReason) {
        self.outcome = CronOutcome::Skipped;
        self.skip_reason = Some(reason);
    }
}

impl Drop for CronRunGuard {
    fn drop(&mut self) {
        let rss_mb_end = current_rss_mb();
        let threads_end = proc_self_thread_count();
        let rss_mb_delta = match (self.rss_mb_start, rss_mb_end) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        };
        let threads_delta = match (self.threads_start, threads_end) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        };
        self.writer.record(CronRunRecord {
            job_name: std::mem::take(&mut self.job_name),
            trigger_source: self.trigger_source,
            outcome: self.outcome,
            skip_reason: self.skip_reason,
            error_detail: self.error_detail.take(),
            project: self.project.take(),
            started_at: self.started_at,
            completed_at: Utc::now(),
            duration_ms: self.start_instant.elapsed().as_millis() as i64,
            rss_mb_start: self.rss_mb_start,
            rss_mb_end,
            rss_mb_delta,
            threads_start: self.threads_start,
            threads_end,
            threads_delta,
            counters: std::mem::replace(&mut self.counters, Value::Null),
        });
    }
}

/// Spawn a light cron's async body on `rt` wrapped in a [`CronRunGuard`], so it
/// records a `cron_run_history` row (Ok on completion, Panicked if it unwinds)
/// with duration / RSS / thread intrinsics. The uniform recorder for the
/// `rt.spawn(async { run_or_log(...).await })` light-cron pattern; mirrors how
/// the `run_or_log` heavy crons record `ok()` after their (error-swallowing)
/// body. Fire-and-forget, like the bare `rt.spawn` it replaces.
pub fn spawn_recorded<F>(
    rt: &tokio::runtime::Handle,
    hist: CronHistoryWriter,
    job: &'static str,
    fut: F,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    rt.spawn(async move {
        let mut guard = CronRunGuard::new(hist, job, CronTriggerSource::Scheduled, None);
        fut.await;
        guard.ok();
    });
}

/// Like [`spawn_recorded`], but the body yields a `serde_json::Value` of
/// job-specific counters that are recorded into the `cron_run_history.counters`
/// JSONB via [`CronRunGuard::ok_with`] — so a light cron's reclaimed/processed
/// counts are queryable through `cron_history` instead of an empty `{}`. Used by
/// `target-cleanup` / `docker-cleanup`, whose bodies return their reclamation
/// report rendered with `to_counters()`.
pub fn spawn_recorded_with<F>(
    rt: &tokio::runtime::Handle,
    hist: CronHistoryWriter,
    job: &'static str,
    fut: F,
) where
    F: std::future::Future<Output = Value> + Send + 'static,
{
    rt.spawn(async move {
        let mut guard = CronRunGuard::new(hist, job, CronTriggerSource::Scheduled, None);
        let counters = fut.await;
        guard.ok_with(counters);
    });
}

/// Spawn the writer task on the current tokio runtime. Returns the cloneable
/// writer handle (stored on `SystemContext`) and the task `JoinHandle` (the
/// daemon awaits it briefly on graceful shutdown to flush the final batch).
/// `cancel` is the daemon's shutdown token.
pub fn spawn_cron_history_writer(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    cancel: CancellationToken,
) -> (CronHistoryWriter, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<CronRunRecord>(CRON_HISTORY_CHANNEL_CAPACITY);
    let writer = CronHistoryWriter {
        tx: Some(tx),
        stats: Arc::clone(&stats),
    };
    info!("cron history writer task starting");
    let handle = tokio::spawn(run_cron_history_writer(pool, stats, rx, cancel));
    (writer, handle)
}

async fn run_cron_history_writer(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    mut rx: mpsc::Receiver<CronRunRecord>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let mut batch = Vec::new();
                while let Ok(r) = rx.try_recv() {
                    batch.push(r);
                    if batch.len() >= MAX_BATCH {
                        flush_batch(&pool, &stats, &batch).await;
                        batch.clear();
                    }
                }
                if !batch.is_empty() {
                    flush_batch(&pool, &stats, &batch).await;
                }
                info!("cron history writer exited");
                return;
            }
            maybe = rx.recv() => match maybe {
                Some(first) => {
                    let mut batch = vec![first];
                    while batch.len() < MAX_BATCH {
                        match rx.try_recv() {
                            Ok(r) => batch.push(r),
                            Err(_) => break,
                        }
                    }
                    flush_batch(&pool, &stats, &batch).await;
                }
                None => {
                    debug!("cron history channel closed");
                    return;
                }
            },
        }
    }
}

async fn flush_batch(pool: &PgPool, stats: &StatsTracker, rows: &[CronRunRecord]) {
    if rows.is_empty() {
        return;
    }
    let n = rows.len();
    let mut job_names = Vec::with_capacity(n);
    let mut trigger_sources = Vec::with_capacity(n);
    let mut outcomes = Vec::with_capacity(n);
    let mut skip_reasons: Vec<Option<String>> = Vec::with_capacity(n);
    let mut error_details: Vec<Option<String>> = Vec::with_capacity(n);
    let mut projects: Vec<Option<String>> = Vec::with_capacity(n);
    let mut started: Vec<DateTime<Utc>> = Vec::with_capacity(n);
    let mut completed: Vec<DateTime<Utc>> = Vec::with_capacity(n);
    let mut durations: Vec<i64> = Vec::with_capacity(n);
    let mut rss_start: Vec<Option<i64>> = Vec::with_capacity(n);
    let mut rss_end: Vec<Option<i64>> = Vec::with_capacity(n);
    let mut rss_delta: Vec<Option<i64>> = Vec::with_capacity(n);
    let mut th_start: Vec<Option<i32>> = Vec::with_capacity(n);
    let mut th_end: Vec<Option<i32>> = Vec::with_capacity(n);
    let mut th_delta: Vec<Option<i32>> = Vec::with_capacity(n);
    let mut counters: Vec<Value> = Vec::with_capacity(n);
    for r in rows {
        job_names.push(r.job_name.clone());
        trigger_sources.push(r.trigger_source.as_str().to_string());
        outcomes.push(r.outcome.as_str().to_string());
        skip_reasons.push(r.skip_reason.map(|s| s.as_str().to_string()));
        error_details.push(r.error_detail.clone());
        projects.push(r.project.clone());
        started.push(r.started_at);
        completed.push(r.completed_at);
        durations.push(r.duration_ms);
        rss_start.push(r.rss_mb_start);
        rss_end.push(r.rss_mb_end);
        rss_delta.push(r.rss_mb_delta);
        th_start.push(r.threads_start);
        th_end.push(r.threads_end);
        th_delta.push(r.threads_delta);
        counters.push(r.counters.clone());
    }

    let sql = "INSERT INTO cron_run_history
        (job_name, trigger_source, outcome, skip_reason, error_detail, project,
         started_at, completed_at, duration_ms,
         rss_mb_start, rss_mb_end, rss_mb_delta,
         threads_start, threads_end, threads_delta, counters)
        SELECT * FROM UNNEST(
            $1::text[],  $2::text[],  $3::text[],  $4::text[],  $5::text[],  $6::text[],
            $7::timestamptz[], $8::timestamptz[], $9::bigint[],
            $10::bigint[], $11::bigint[], $12::bigint[],
            $13::int[], $14::int[], $15::int[], $16::jsonb[]
        )";

    let result = sqlx::query(sql)
        .bind(&job_names)
        .bind(&trigger_sources)
        .bind(&outcomes)
        .bind(&skip_reasons)
        .bind(&error_details)
        .bind(&projects)
        .bind(&started)
        .bind(&completed)
        .bind(&durations)
        .bind(&rss_start)
        .bind(&rss_end)
        .bind(&rss_delta)
        .bind(&th_start)
        .bind(&th_end)
        .bind(&th_delta)
        .bind(&counters)
        .execute(pool)
        .await;
    match result {
        Ok(r) => debug!(rows = r.rows_affected(), "cron history batch flushed"),
        Err(e) => {
            error!(rows = n, error = %e, "cron history batch flush failed");
            stats
                .cron_history_writes_dropped
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::Receiver<CronRunRecord>) -> Vec<CronRunRecord> {
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push(r);
        }
        out
    }

    fn test_writer() -> (CronHistoryWriter, mpsc::Receiver<CronRunRecord>) {
        let (tx, rx) = mpsc::channel::<CronRunRecord>(64);
        let writer = CronHistoryWriter {
            tx: Some(tx),
            stats: Arc::new(StatsTracker::new()),
        };
        (writer, rx)
    }

    #[test]
    fn guard_ok_records_one_ok_row() {
        let (writer, mut rx) = test_writer();
        {
            let mut g = CronRunGuard::new(
                writer,
                "topic-clustering",
                CronTriggerSource::Scheduled,
                None,
            );
            g.ok();
        }
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CronOutcome::Ok);
        assert_eq!(rows[0].job_name, "topic-clustering");
        assert_eq!(rows[0].skip_reason, None);
    }

    #[test]
    fn guard_drop_without_finisher_records_panicked() {
        let (writer, mut rx) = test_writer();
        drop(CronRunGuard::new(
            writer,
            "call-graph",
            CronTriggerSource::Scheduled,
            None,
        ));
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CronOutcome::Panicked);
    }

    #[test]
    fn guard_fail_carries_detail() {
        let (writer, mut rx) = test_writer();
        {
            let mut g = CronRunGuard::new(
                writer,
                "symbol-extraction",
                CronTriggerSource::Manual,
                Some("pgmcp".into()),
            );
            g.fail("boom");
        }
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CronOutcome::Failed);
        assert_eq!(rows[0].error_detail.as_deref(), Some("boom"));
        assert_eq!(rows[0].trigger_source, CronTriggerSource::Manual);
        assert_eq!(rows[0].project.as_deref(), Some("pgmcp"));
    }

    #[test]
    fn guard_ok_with_carries_counters() {
        let (writer, mut rx) = test_writer();
        {
            let mut g =
                CronRunGuard::new(writer, "graph-analysis", CronTriggerSource::Scheduled, None);
            g.ok_with(serde_json::json!({"edges": 7}));
        }
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CronOutcome::Ok);
        assert_eq!(rows[0].counters["edges"], 7);
    }

    #[test]
    fn spawn_recorded_with_persists_body_counters() {
        let (writer, mut rx) = test_writer();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            spawn_recorded_with(
                &tokio::runtime::Handle::current(),
                writer,
                "target-cleanup",
                async { serde_json::json!({"total_bytes": 55, "reap_files": 2818}) },
            );
            // The body is immediately ready, so yielding lets the spawned task run
            // to completion (body → ok_with → guard drop → record) before we drain.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        });
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "exactly one row recorded");
        assert_eq!(rows[0].outcome, CronOutcome::Ok);
        assert_eq!(rows[0].job_name, "target-cleanup");
        assert_eq!(rows[0].counters["total_bytes"], 55);
        assert_eq!(rows[0].counters["reap_files"], 2818);
    }

    #[test]
    fn record_skip_emits_a_skipped_row() {
        let (writer, mut rx) = test_writer();
        writer.record_skip(
            "fuzzy-sync",
            CronTriggerSource::Scheduled,
            SkipReason::Cooldown,
        );
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CronOutcome::Skipped);
        assert_eq!(rows[0].skip_reason, Some(SkipReason::Cooldown));
    }

    #[test]
    fn null_writer_is_a_noop() {
        let writer = CronHistoryWriter::null(Arc::new(StatsTracker::new()));
        // Neither path panics nor blocks.
        writer.record_skip("x", CronTriggerSource::Startup, SkipReason::DbDown);
        let mut g = CronRunGuard::new(writer, "x", CronTriggerSource::Startup, None);
        g.ok();
    }
}
