//! Generic Lock-Free Reactive State Machine Task Scheduler
//!
//! Adapted from MeTTaTron's task_scheduler.rs.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use parking_lot::Mutex;
use tracing::{error, warn};

use crate::config::CronConfig;
use crate::daemon_state::DaemonLifecycle;
use crate::embed::pool::{EmbedCommitRequest, EmbedIndexRequest};
mod state;
pub use state::*;

use crate::stats::tracker::StatsTracker;

// ============================================================================
// Time utilities
// ============================================================================

pub type UnixTimestampMs = u64;

/// Compute a per-job initial-delay offset that scatters heavy crons
/// across their interval window. Without this, every cron with
/// `initial_delay_ms = 1_000` fires together at `T+1s` on startup AND
/// at every multiple of its interval thereafter — producing collision
/// windows at LCM(interval₁, interval₂, …) for which the 20-slot DB
/// pool starved (observed 2026-05-21 between 19:11 and 19:25 when
/// similarity-scan @6h, graph-analysis @2h, symbol-extraction @2h,
/// and call-graph @2h all converged near a multiple of 2h).
///
/// The offset is `base_ms + (hash(job_name) % cap_ms)` where the cap
/// is `min(interval_ms / 2, 600_000)` — at most 10 minutes of jitter,
/// or half the interval for very fast crons. Hash is FxHash-style
/// (multiplicative + rotate), deterministic across runs so a given
/// job always starts at the same offset relative to daemon start.
fn staggered_initial_delay_ms(job_name: &str, interval_ms: u64) -> u64 {
    const BASE_MS: u64 = 1_000;
    const MAX_JITTER_MS: u64 = 600_000; // 10 minutes
    let cap = MAX_JITTER_MS.min(interval_ms / 2).max(1);
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for b in job_name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3); // FNV-1a prime
    }
    BASE_MS + (h % cap)
}

/// RAII guard that flips `stats.heavy_cron_running` → true on construction
/// and back to false on drop. Used by the four heavy cron bodies so the
/// Prometheus `pgmcp_heavy_cron_running` gauge reflects live state regardless
/// of early-return or panic.
struct HeavyCronFlag {
    stats: Arc<StatsTracker>,
}

impl HeavyCronFlag {
    fn new(stats: Arc<StatsTracker>) -> Self {
        stats
            .heavy_cron_running
            .store(true, AtomicOrdering::Release);
        Self { stats }
    }
}

impl Drop for HeavyCronFlag {
    fn drop(&mut self) {
        self.stats
            .heavy_cron_running
            .store(false, AtomicOrdering::Release);
    }
}

/// Heavy-cron skip-gate macro. Returns either a held `MutexGuard` (work
/// can proceed) or causes the enclosing closure to `return;` after
/// recording the exact `SkipReason` into `stats.last_cron_outcomes` so
/// an operator can tell which gate is silencing each cron.
///
/// Without this, the seven heavy-cron closures silently `return;` at
/// each gate and the scheduler records `CronJobOutcome::Ok` (because
/// the *closure* returned cleanly). Bug C in the 2026-05-21 staleness
/// investigation: 363 cron_executions with zero work-counters because
/// every tick hit one of these three gates.
macro_rules! heavy_gate_or_skip {
    (
        job = $job:literal,
        lc = $lc:expr,
        ready = $ready:expr,
        cooldown = $cooldown:expr,
        lock = $lock:expr,
        stats = $stats:expr,
    ) => {{
        use crate::stats::tracker::{CronJobOutcome, SkipReason};
        if !$lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
            $stats.record_cron_outcome($job, CronJobOutcome::Skipped(SkipReason::PhaseGate), 0);
            return;
        }
        // The PhaseGate check passes through `Terminating` because
        // `DaemonPhase` is ordered Initializing < Scanning < Ready <
        // Terminating < Defunct (`src/daemon_state.rs:35-46`). Without
        // this second gate, closures already enqueued at SIGTERM race
        // the closing PG pool / channels and the next pool-acquire
        // logs "attempted to acquire a connection on a closed pool"
        // (47× across one shutdown in the 2026-05-25 log triage). See
        // plan ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
        // F3.
        if $lc.is_stopping() {
            $stats.record_cron_outcome($job, CronJobOutcome::Skipped(SkipReason::Shutdown), 0);
            return;
        }
        let first_seen = $ready.get_or_init(Instant::now);
        if first_seen.elapsed() < $cooldown {
            tracing::debug!(
                job = $job,
                elapsed_ms = first_seen.elapsed().as_millis() as u64,
                cooldown_ms = $cooldown.as_millis() as u64,
                phase_ms = $lc.ms_in_current_phase(),
                "heavy cron in ready-relative cooldown"
            );
            $stats.record_cron_outcome($job, CronJobOutcome::Skipped(SkipReason::Cooldown), 0);
            return;
        }
        match $lock.try_lock() {
            Some(g) => g,
            None => {
                tracing::info!(concat!("heavy cron busy, deferring ", $job));
                $stats.record_cron_outcome($job, CronJobOutcome::Skipped(SkipReason::LockBusy), 0);
                return;
            }
        }
    }};
}

#[inline]
pub fn now_ms() -> UnixTimestampMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time went backwards")
        .as_millis() as u64
}

// ============================================================================
// TaskMetadata
// ============================================================================

#[derive(Clone, Debug)]
pub enum TaskMetadata {
    OneShot,
    Recurring {
        interval_ms: u64,
    },
    Named {
        name: String,
        recurring_interval_ms: Option<u64>,
    },
}

impl TaskMetadata {
    #[inline]
    pub fn recurrence_interval(&self) -> Option<u64> {
        match self {
            TaskMetadata::OneShot => None,
            TaskMetadata::Recurring { interval_ms } => Some(*interval_ms),
            TaskMetadata::Named {
                recurring_interval_ms,
                ..
            } => *recurring_interval_ms,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TaskMetadata::OneShot => "one-shot",
            TaskMetadata::Recurring { .. } => "recurring",
            TaskMetadata::Named { name, .. } => name,
        }
    }
}

// ============================================================================
// ScheduledTask
// ============================================================================

pub struct ScheduledTask {
    pub scheduled_time_ms: UnixTimestampMs,
    pub metadata: TaskMetadata,
    pub task: Box<dyn FnMut() -> bool + Send>,
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("scheduled_time_ms", &self.scheduled_time_ms)
            .field("metadata", &self.metadata)
            .field("task", &"<fn>")
            .finish()
    }
}

impl Ord for ScheduledTask {
    fn cmp(&self, other: &Self) -> Ordering {
        other.scheduled_time_ms.cmp(&self.scheduled_time_ms)
    }
}

impl PartialOrd for ScheduledTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ScheduledTask {
    fn eq(&self, other: &Self) -> bool {
        self.scheduled_time_ms == other.scheduled_time_ms
    }
}

impl Eq for ScheduledTask {}

// ============================================================================
// CronStateMachine
// ============================================================================

pub struct CronStateMachine {
    state: CronState,
    queue: BinaryHeap<ScheduledTask>,
    task_rx: Receiver<ScheduledTask>,
    poll_interval_ms: u64,
    terminating: Arc<AtomicBool>,
    channel_disconnected: bool,
    ready_tx: Option<Sender<()>>,
    stats: Option<Arc<StatsTracker>>,
}

impl CronStateMachine {
    pub const DEFAULT_POLL_INTERVAL_MS: u64 = 100;

    pub fn new(
        task_rx: Receiver<ScheduledTask>,
        terminating: Arc<AtomicBool>,
        poll_interval_ms: u64,
        ready_tx: Option<Sender<()>>,
        stats: Option<Arc<StatsTracker>>,
    ) -> Self {
        Self {
            state: CronState::CheckEvents,
            queue: BinaryHeap::new(),
            task_rx,
            poll_interval_ms,
            terminating,
            channel_disconnected: false,
            ready_tx,
            stats,
        }
    }

    pub fn run(&mut self) {
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(());
        }

        while self.state != CronState::Terminated {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let event = self.poll_event();
                self.transition(event);
            }));

            if let Err(payload) = result {
                error!(
                    panic = ?payload,
                    "CronStateMachine::run() caught panic -- resetting to CheckEvents"
                );
                self.state = CronState::CheckEvents;
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn poll_event(&mut self) -> CronEvent {
        if let Some(task) = self.queue.peek()
            && task.scheduled_time_ms <= now_ms()
        {
            return CronEvent::TaskDue;
        }

        if self.terminating.load(AtomicOrdering::Acquire) {
            return CronEvent::TerminationRequested;
        }

        match self.state {
            CronState::CheckEvents => self.poll_check_events(),
            CronState::DrainChannel => self.poll_drain_channel(),
            CronState::ExecutingTask => unreachable!("ExecutingTask polls internally"),
            CronState::Sleeping => CronEvent::TimerExpired,
            CronState::Terminated => unreachable!("Cannot poll from Terminated"),
        }
    }

    fn poll_check_events(&mut self) -> CronEvent {
        if self.terminating.load(AtomicOrdering::Acquire) {
            return CronEvent::TerminationRequested;
        }

        match self.task_rx.try_recv() {
            Ok(task) => {
                self.queue.push(task);
                return CronEvent::TaskReceived;
            }
            Err(TryRecvError::Disconnected) if !self.channel_disconnected => {
                self.channel_disconnected = true;
                return CronEvent::ChannelDisconnected;
            }
            _ => {}
        }

        if let Some(task) = self.queue.peek()
            && task.scheduled_time_ms <= now_ms()
        {
            return CronEvent::TaskDue;
        }

        CronEvent::NoEvents
    }

    fn poll_drain_channel(&mut self) -> CronEvent {
        match self.task_rx.try_recv() {
            Ok(task) => {
                self.queue.push(task);
                CronEvent::TaskReceived
            }
            Err(TryRecvError::Empty) => {
                if let Some(task) = self.queue.peek()
                    && task.scheduled_time_ms <= now_ms()
                {
                    return CronEvent::TaskDue;
                }
                CronEvent::NoEvents
            }
            Err(TryRecvError::Disconnected) => {
                self.channel_disconnected = true;
                CronEvent::ChannelDisconnected
            }
        }
    }

    fn transition(&mut self, event: CronEvent) {
        self.state = match (self.state, event) {
            (_, CronEvent::TerminationRequested) => CronState::Terminated,

            (CronState::CheckEvents, CronEvent::TaskReceived) => CronState::DrainChannel,
            (CronState::CheckEvents, CronEvent::TaskDue) => {
                self.execute_one_task();
                CronState::CheckEvents
            }
            (CronState::CheckEvents, CronEvent::NoEvents) => CronState::Sleeping,
            (CronState::CheckEvents, CronEvent::ChannelDisconnected) => {
                if self.queue.is_empty() {
                    CronState::Terminated
                } else {
                    CronState::CheckEvents
                }
            }

            (CronState::DrainChannel, CronEvent::TaskReceived) => CronState::DrainChannel,
            (CronState::DrainChannel, CronEvent::TaskDue) => {
                self.execute_one_task();
                CronState::CheckEvents
            }
            (CronState::DrainChannel, CronEvent::NoEvents) => CronState::Sleeping,
            (CronState::DrainChannel, CronEvent::ChannelDisconnected) => CronState::CheckEvents,

            (CronState::Sleeping, CronEvent::TimerExpired) => CronState::CheckEvents,
            (CronState::Sleeping, CronEvent::TaskDue) => {
                self.execute_one_task();
                CronState::CheckEvents
            }

            (CronState::ExecutingTask, CronEvent::TaskCompleted { .. }) => CronState::CheckEvents,

            (CronState::Terminated, _) => unreachable!("Cannot transition from Terminated"),

            (state, event) => {
                warn!(?state, ?event, "Unexpected transition");
                CronState::CheckEvents
            }
        };

        if self.state == CronState::Sleeping {
            self.do_sleep();
        }
    }

    fn execute_one_task(&mut self) {
        let Some(task) = self.queue.pop() else {
            return;
        };
        Self::execute_inline(task, &mut self.queue, &self.stats);
    }

    fn execute_inline(
        mut task: ScheduledTask,
        queue: &mut BinaryHeap<ScheduledTask>,
        stats: &Option<Arc<StatsTracker>>,
    ) {
        let task_name = task.metadata.name().to_string();

        // Skip-check: if a previous run of this cron body classified a
        // permanent fault via `CronAction::Disable`, don't re-run.
        // Re-queue the recurrence so the scheduler keeps tracking the
        // job (and operators can see the "disabled" state next to the
        // last-known outcome), but don't burn cycles invoking it.
        if let Some(s) = stats
            && s.is_cron_job_disabled(&task_name)
        {
            if let Some(interval) = task.metadata.recurrence_interval() {
                task.scheduled_time_ms = now_ms() + interval;
                queue.push(task);
            }
            return;
        }

        let started = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (task.task)()));
        let elapsed_ms = started.elapsed().as_millis() as u64;

        if let Some(s) = stats {
            s.cron_executions.fetch_add(1, AtomicOrdering::Relaxed);
        }

        match result {
            Ok(should_requeue) => {
                if let Some(s) = stats {
                    s.record_cron_outcome(
                        &task_name,
                        crate::stats::tracker::CronJobOutcome::Ok,
                        elapsed_ms,
                    );
                }
                if should_requeue && let Some(interval) = task.metadata.recurrence_interval() {
                    task.scheduled_time_ms = now_ms() + interval;
                    queue.push(task);
                }
            }
            Err(e) => {
                error!(task_name = %task_name, panic = ?e, "Task panicked");
                if let Some(s) = stats {
                    s.cron_panics.fetch_add(1, AtomicOrdering::Relaxed);
                    s.record_cron_outcome(
                        &task_name,
                        crate::stats::tracker::CronJobOutcome::Panicked,
                        elapsed_ms,
                    );
                }
            }
        }
    }

    fn do_sleep(&self) {
        let sleep_ms = if let Some(task) = self.queue.peek() {
            let now = now_ms();
            if task.scheduled_time_ms <= now {
                0
            } else {
                (task.scheduled_time_ms - now).min(self.poll_interval_ms)
            }
        } else {
            self.poll_interval_ms
        };

        if sleep_ms > 0 {
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    }

    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }

    pub fn current_state(&self) -> CronState {
        self.state
    }
}

// ============================================================================
// CronHandle
// ============================================================================

#[derive(Clone)]
pub struct CronHandle {
    task_tx: Sender<ScheduledTask>,
    terminating: Arc<AtomicBool>,
}

impl CronHandle {
    pub fn schedule_at<F>(&self, time_ms: UnixTimestampMs, metadata: TaskMetadata, task: F) -> bool
    where
        F: FnMut() -> bool + Send + 'static,
    {
        let scheduled_task = ScheduledTask {
            scheduled_time_ms: time_ms,
            metadata,
            task: Box::new(task),
        };
        self.task_tx.send(scheduled_task).is_ok()
    }

    pub fn schedule_after<F>(&self, delay_ms: u64, metadata: TaskMetadata, task: F) -> bool
    where
        F: FnMut() -> bool + Send + 'static,
    {
        self.schedule_at(now_ms() + delay_ms, metadata, task)
    }

    pub fn schedule_recurring<F>(
        &self,
        initial_delay_ms: u64,
        interval_ms: u64,
        name: &str,
        task: F,
    ) -> bool
    where
        F: FnMut() -> bool + Send + 'static,
    {
        let metadata = TaskMetadata::Named {
            name: name.to_string(),
            recurring_interval_ms: Some(interval_ms),
        };
        self.schedule_after(initial_delay_ms, metadata, task)
    }

    pub fn schedule_once<F>(&self, delay_ms: u64, name: &str, task: F) -> bool
    where
        F: FnMut() -> bool + Send + 'static,
    {
        let metadata = TaskMetadata::Named {
            name: name.to_string(),
            recurring_interval_ms: None,
        };
        self.schedule_after(delay_ms, metadata, task)
    }

    pub fn request_shutdown(&self) {
        self.terminating.store(true, AtomicOrdering::Release);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.terminating.load(AtomicOrdering::Acquire)
    }
}

// ============================================================================
// Spawn functions
// ============================================================================

pub fn spawn_cron(
    terminating: Arc<AtomicBool>,
    stats: Option<Arc<StatsTracker>>,
) -> (CronHandle, JoinHandle<()>, Receiver<()>) {
    spawn_cron_with_interval(
        terminating,
        CronStateMachine::DEFAULT_POLL_INTERVAL_MS,
        stats,
    )
}

pub fn spawn_cron_with_interval(
    terminating: Arc<AtomicBool>,
    poll_interval_ms: u64,
    stats: Option<Arc<StatsTracker>>,
) -> (CronHandle, JoinHandle<()>, Receiver<()>) {
    let (task_tx, task_rx) = unbounded::<ScheduledTask>();
    let (ready_tx, ready_rx) = unbounded::<()>();

    let terminating_clone = Arc::clone(&terminating);

    let thread_handle = thread::Builder::new()
        .name("pgmcp-cron".to_string())
        .spawn(move || {
            let mut sm = CronStateMachine::new(
                task_rx,
                terminating_clone,
                poll_interval_ms,
                Some(ready_tx),
                stats,
            );
            sm.run();
        })
        .expect("Failed to spawn cron state machine thread");

    let handle = CronHandle {
        task_tx,
        terminating,
    };

    (handle, thread_handle, ready_rx)
}

// ============================================================================
// Maintenance job scheduling
// ============================================================================

/// Schedule all standard maintenance cron jobs.
///
/// `rt` must be a handle to the tokio runtime so cron closures (which run on a
/// plain `std::thread`) can spawn async database work. Passing it explicitly
/// avoids the `try_current()` pitfall — `Handle::try_current()` always fails on
/// non-tokio threads.
#[allow(clippy::too_many_arguments)]
pub fn schedule_maintenance_jobs(
    handle: &CronHandle,
    db: Arc<dyn crate::db::DbClient>,
    stats: Arc<StatsTracker>,
    config: &CronConfig,
    fuzzy_config: &crate::config::FuzzyConfig,
    embeddings_config: &crate::config::EmbeddingsConfig,
    rt: tokio::runtime::Handle,
    embed_tx: crossbeam_channel::Sender<EmbedIndexRequest>,
    lifecycle: DaemonLifecycle,
    cron_pool: Arc<crate::work_pool::pool::WorkPool>,
    general_pool: Option<Arc<crate::work_pool::pool::WorkPool>>,
    system_ctx: crate::context::SystemContext,
) {
    // Stats aggregation (light — runs unconditionally)
    let stats_clone = Arc::clone(&stats);
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    handle.schedule_recurring(
        1000, // 1s initial delay
        config.stats_aggregation_interval_secs * 1000,
        "stats-aggregation",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            let stats = Arc::clone(&stats_clone);
            rt_clone.spawn(async move {
                if let Ok(count) = db.count_indexed_files().await {
                    stats
                        .files_indexed
                        .store(count, std::sync::atomic::Ordering::Relaxed);
                }
            });
            true
        },
    );

    // Stale file cleanup + orphaned project cleanup (light — runs unconditionally)
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    handle.schedule_recurring(
        5000,
        config.stale_cleanup_interval_secs * 1000,
        "stale-cleanup",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            rt_clone.spawn(async move {
                match db.cleanup_stale_files().await {
                    Ok(count) => {
                        if count > 0 {
                            tracing::info!(count, "Cleaned up stale files");
                        }
                    }
                    Err(e) => tracing::error!("Stale cleanup failed: {}", e),
                }
                // Clean up projects left with zero indexed files
                match db.cleanup_orphaned_projects().await {
                    Ok(count) => {
                        if count > 0 {
                            tracing::info!(count, "Cleaned up orphaned projects");
                        }
                    }
                    Err(e) => tracing::error!("Orphaned project cleanup failed: {}", e),
                }
            });
            true
        },
    );

    // Work-item presence + lease decay (light — runs unconditionally, like
    // stale-cleanup). Releases expired claims (A2A crash-safety: a dead agent's
    // claims become stealable) and decays agent_presence active→idle→offline.
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let stats_clone = Arc::clone(&stats);
    let lc = lifecycle.clone();
    let presence_interval = config.work_item_presence_interval_secs;
    let presence_idle = config.work_item_presence_idle_secs as i64;
    let presence_offline = config.work_item_presence_offline_secs as i64;
    handle.schedule_recurring(
        staggered_initial_delay_ms("work-item-presence", presence_interval * 1000),
        presence_interval * 1000,
        "work-item-presence",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let stats = Arc::clone(&stats_clone);
            if let Some(pool) = db_clone.pool().cloned() {
                rt_clone.spawn(async move {
                    crate::cron::work_item_presence::run_or_log(
                        pool,
                        stats,
                        presence_idle,
                        presence_offline,
                    )
                    .await;
                });
            }
            true
        },
    );

    // findings-promotion (Phase 3): idempotently materialize high-confidence
    // bug_prediction / high-severity documented_tech_debt findings into
    // `pending` work items, for projects that opt in via
    // `[tracker] auto_promote_findings`. Light job (bounded per-project queries
    // + the shared scan/scoring primitives) — runs on the runtime like the
    // presence sweep, no heavy-cron gate. Interval-gated (0 disables globally).
    if config.findings_promotion_interval_secs > 0 {
        let db_clone_fp = Arc::clone(&db);
        let rt_clone_fp = rt.clone();
        let stats_clone_fp = Arc::clone(&stats);
        let lc_fp = lifecycle.clone();
        let fp_interval = config.findings_promotion_interval_secs;
        handle.schedule_recurring(
            staggered_initial_delay_ms("findings-promotion", fp_interval * 1000),
            fp_interval * 1000,
            "findings-promotion",
            move || {
                if lc_fp.is_stopping() {
                    return false;
                }
                let stats = Arc::clone(&stats_clone_fp);
                if let Some(pool) = db_clone_fp.pool().cloned() {
                    rt_clone_fp.spawn(async move {
                        crate::cron::findings_promotion::run_or_log(pool, stats).await;
                    });
                }
                true
            },
        );
    }

    // Integrity check: clean up files with incomplete indexing (NULL content_hash).
    // These are files where pgmcp was killed between upsert and embedding completion.
    // Deleting them causes re-indexing on the next scan; ON DELETE CASCADE cleans partial chunks.
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    handle.schedule_recurring(
        config.integrity_check_interval_secs * 1000,
        config.integrity_check_interval_secs * 1000,
        "integrity-check",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            rt_clone.spawn(async move {
                match sqlx::query("DELETE FROM indexed_files WHERE content_hash IS NULL")
                    .execute(db.pool().expect("inline SQL needs PgPool"))
                    .await
                {
                    Ok(result) => {
                        let count = result.rows_affected();
                        if count > 0 {
                            tracing::info!(count, "Cleaned up incompletely indexed files");
                        }
                    }
                    Err(e) => tracing::error!("Integrity check failed: {}", e),
                }
            });
            true
        },
    );

    // DB maintenance (VACUUM ANALYZE) (light — runs unconditionally)
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    handle.schedule_recurring(
        config.db_maintenance_interval_secs * 1000,
        config.db_maintenance_interval_secs * 1000,
        "db-maintenance",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            rt_clone.spawn(async move {
                if let Err(e) = sqlx::query("VACUUM ANALYZE indexed_files")
                    .execute(db.pool().expect("inline SQL needs PgPool"))
                    .await
                {
                    tracing::error!("DB maintenance failed: {}", e);
                }
                if let Err(e) = sqlx::query("VACUUM ANALYZE file_chunks")
                    .execute(db.pool().expect("inline SQL needs PgPool"))
                    .await
                {
                    tracing::error!("DB maintenance (chunks) failed: {}", e);
                }
            });
            true
        },
    );

    // Git history indexing (heavy — gates on Ready)
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();

    // Create a commit-specific sender by wrapping EmbedCommitRequest → EmbedIndexRequest
    let (commit_tx, commit_rx) = crossbeam_channel::bounded::<EmbedCommitRequest>(64);
    let kind_tx = embed_tx;
    std::thread::Builder::new()
        .name("pgmcp-git-embed-adapter".into())
        .spawn(move || {
            for req in commit_rx {
                if kind_tx.send(EmbedIndexRequest::Commit(req)).is_err() {
                    break;
                }
            }
        })
        .expect("Failed to spawn git embed adapter thread");

    // Heavy crons coordinate via a single mutex so at most one runs at a time.
    // Each job also tracks "first time we saw Ready" via its own OnceLock so the
    // initial-fire delay is relative to Ready (the daemon's scan completion),
    // not to scheduler start. This prevents the old pile-up where all four
    // overdue heavy crons fired within the same 60s window after Ready.
    let heavy_cron_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    // Dedicated lock for the cheap, read-mostly quality-history snapshot. It was
    // previously on `heavy_cron_lock`, where it lost the try_lock race to the
    // 20-30 min GPU jobs (topic-clustering / graph-analysis) every tick and
    // starved (observed: `quality_history_runs=1`, then perpetual
    // `skipped:lock_busy`), leaving quality_trend / quality_forecast / burndown
    // and the digest with an empty series. It only reads tables (MVCC-safe
    // alongside the GPU writers) and appends to quality_report_history, so it
    // needs to exclude only itself — its own lock decouples it from the GPU herd.
    let quality_history_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

    let stats_for_git = Arc::clone(&stats);
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_git: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let git_ready_delay = Duration::from_secs(config.ready_delay_git_secs);
    let cron_pool_git = Arc::clone(&cron_pool);
    let db_for_git = db_clone.clone();
    let commit_tx_for_git = commit_tx.clone();
    let rt_for_git = rt_clone.clone();
    handle.schedule_recurring(
        // 1s base delay + per-job hash jitter so this heavy cron doesn't
        // collide with the others at startup. The real wait still
        // happens on Ready-relative check below.
        staggered_initial_delay_ms(
            "git-history-index",
            config.git_history_index_interval_secs * 1000,
        ),
        config.git_history_index_interval_secs * 1000,
        "git-history-index",
        move || {
            if lc.is_stopping() {
                return false;
            }
            // Dispatch the body to CronPool so the scheduler thread is
            // never blocked by a heavy `block_on`. The shared
            // `heavy_cron_lock` continues to serialize the heavy quartet.
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_git = Arc::clone(&ready_git);
            let stats_for_git = Arc::clone(&stats_for_git);
            let db = db_for_git.clone();
            let tx = commit_tx_for_git.clone();
            let rt = rt_for_git.clone();
            cron_pool_git.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "git-history-index",
                        lc = lc,
                        ready = ready_git,
                        cooldown = git_ready_delay,
                        lock = lock,
                        stats = stats_for_git,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_git));
                    let stats = Arc::clone(&stats_for_git);

                    // Counter is at top-of-body: a reliable "this cron's
                    // body ran" signal regardless of whether any project
                    // had new commits. Pairs with
                    // `git_history_noop_returns` to distinguish the
                    // empty-data case from "never ran".
                    stats.git_history_runs.fetch_add(1, AtomicOrdering::Relaxed);

                    // Once-per-tick `git` binary preflight. A missing `git`
                    // is a permanent fault for this cron — keep retrying
                    // would just log-spam at every interval until daemon
                    // restart. `classify_io_error` maps `NotFound` to
                    // `Disable`; we record the reason on the stats tracker
                    // so the scheduler skip-check elides future runs.
                    if let Err(io_err) = std::process::Command::new("git").arg("--version").output()
                        && crate::cron::shutdown::classify_io_error(&io_err)
                            == crate::cron::shutdown::CronAction::Disable
                    {
                        let reason = format!("git binary unavailable: {io_err}");
                        tracing::error!(
                            job = "git-history-index",
                            error = %io_err,
                            "permanent fault — disabling cron job until daemon restart"
                        );
                        stats.disable_cron_job("git-history-index", reason);
                        return;
                    }

                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        match db.get_git_enabled_projects().await {
                            Ok(projects) if projects.is_empty() => {
                                stats
                                    .git_history_noop_returns
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                                tracing::info!(
                                    "git-history-index: no git-enabled projects, nothing to do"
                                );
                            }
                            Ok(projects) => {
                                for (project_id, project_path) in &projects {
                                    let project_root = std::path::Path::new(project_path);
                                    if !crate::indexer::git_indexer::is_git_history_enabled(
                                        project_root,
                                    ) {
                                        continue;
                                    }
                                    if let Err(e) = crate::indexer::git_indexer::index_git_history(
                                        project_root,
                                        *project_id,
                                        &db,
                                        &tx,
                                        &stats,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            project = %project_path,
                                            error = %e,
                                            "Git history indexing failed"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to list projects for git indexing: {}", e);
                            }
                        }
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "git-history-index",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // Cross-project similarity scan (heavy — gates on Ready + heavy_cron_lock)
    let db_clone_sim = Arc::clone(&db);
    let rt_clone_sim = rt.clone();
    let stats_for_sim = Arc::clone(&stats);
    let sim_interval = config.similarity_scan_interval_secs;
    let sim_cron_config = CronConfig {
        similarity_scan_interval_secs: sim_interval,
        similarity_threshold: config.similarity_threshold,
        similarity_top_k: config.similarity_top_k,
        topic_scan_interval_secs: config.topic_scan_interval_secs,
        topic_min_cluster_size: config.topic_min_cluster_size,
        topic_num_clusters: config.topic_num_clusters,
        topic_fuzziness: config.topic_fuzziness,
        topic_fcm_max_iters: config.topic_fcm_max_iters,
        topic_fcm_tolerance: config.topic_fcm_tolerance,
        topic_membership_threshold: config.topic_membership_threshold,
        topic_label_top_k: config.topic_label_top_k,
        ..CronConfig::default()
    };
    let sim_ef_search = 100; // default ef_search
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_sim: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let sim_ready_delay = Duration::from_secs(config.ready_delay_similarity_secs);
    let cron_pool_sim = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("similarity-scan", sim_interval * 1000),
        sim_interval * 1000,
        "similarity-scan",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_sim = Arc::clone(&ready_sim);
            let stats_for_sim = Arc::clone(&stats_for_sim);
            let db = db_clone_sim.clone();
            let cfg = sim_cron_config.clone();
            let rt = rt_clone_sim.clone();
            cron_pool_sim.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "similarity-scan",
                        lc = lc,
                        ready = ready_sim,
                        cooldown = sim_ready_delay,
                        lock = lock,
                        stats = stats_for_sim,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_sim));
                    let stats = Arc::clone(&stats_for_sim);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    let lc_inner = lc.clone();
                    rt.block_on(async {
                        crate::cron::similarity::run_similarity_scan(
                            db.as_ref(),
                            &cfg,
                            sim_ef_search,
                            &stats,
                            &lc_inner,
                        )
                        .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "similarity-scan",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // Semantic-edges materialization (heavy — gates on Ready + heavy_cron_lock).
    // Sequenced (via its ready-delay) before graph-analysis so its edges are
    // present for the blended PageRank / betweenness / community pass.
    let db_clone_sem = Arc::clone(&db);
    let rt_clone_sem = rt.clone();
    let stats_for_sem = Arc::clone(&stats);
    let sem_interval = config.semantic_edge_interval_secs;
    let sem_cron_config = config.clone();
    let sem_ef_search = 100; // default ef_search (mirrors similarity-scan)
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_sem: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let sem_ready_delay = Duration::from_secs(config.ready_delay_semantic_secs);
    let cron_pool_sem = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("semantic-edges", sem_interval * 1000),
        sem_interval * 1000,
        "semantic-edges",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_sem = Arc::clone(&ready_sem);
            let stats_for_sem = Arc::clone(&stats_for_sem);
            let db = db_clone_sem.clone();
            let cfg = sem_cron_config.clone();
            let rt = rt_clone_sem.clone();
            cron_pool_sem.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "semantic-edges",
                        lc = lc,
                        ready = ready_sem,
                        cooldown = sem_ready_delay,
                        lock = lock,
                        stats = stats_for_sem,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_sem));
                    let stats = Arc::clone(&stats_for_sem);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    let lc_inner = lc.clone();
                    rt.block_on(async {
                        crate::cron::semantic_edges::run_semantic_edges(
                            db.as_ref(),
                            &cfg,
                            sem_ef_search,
                            &stats,
                            &lc_inner,
                        )
                        .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "semantic-edges",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // Graph analysis (import extraction, PageRank, betweenness, coupling)
    let db_clone_graph = Arc::clone(&db);
    let rt_clone_graph = rt.clone();
    let stats_for_graph = Arc::clone(&stats);
    let graph_interval = config.graph_analysis_interval_secs;
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_graph: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let graph_ready_delay = Duration::from_secs(config.ready_delay_graph_secs);
    let graph_general_pool = general_pool.clone();
    let cron_pool_graph = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("graph-analysis", graph_interval * 1000),
        graph_interval * 1000,
        "graph-analysis",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_graph = Arc::clone(&ready_graph);
            let stats_for_graph = Arc::clone(&stats_for_graph);
            let db = db_clone_graph.clone();
            let wp = graph_general_pool.clone();
            let rt = rt_clone_graph.clone();
            cron_pool_graph.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "graph-analysis",
                        lc = lc,
                        ready = ready_graph,
                        cooldown = graph_ready_delay,
                        lock = lock,
                        stats = stats_for_graph,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_graph));
                    let stats = Arc::clone(&stats_for_graph);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        crate::cron::graph_analysis::run_graph_analysis(db.as_ref(), &stats, wp)
                            .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "graph-analysis",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // Symbol extraction (Tier-0e tree-sitter pass — populates file_symbols + symbol_references)
    let db_clone_symbol = Arc::clone(&db);
    let rt_clone_symbol = rt.clone();
    let stats_for_symbol = Arc::clone(&stats);
    let symbol_extraction_interval = config.symbol_extraction_interval_secs;
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_symbol_extraction: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let symbol_extraction_ready_delay =
        Duration::from_secs(config.ready_delay_symbol_extraction_secs);
    let cron_pool_symbol = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("symbol-extraction", symbol_extraction_interval * 1000),
        symbol_extraction_interval * 1000,
        "symbol-extraction",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_symbol_extraction = Arc::clone(&ready_symbol_extraction);
            let stats_for_symbol = Arc::clone(&stats_for_symbol);
            let db = db_clone_symbol.clone();
            let rt = rt_clone_symbol.clone();
            cron_pool_symbol.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "symbol-extraction",
                        lc = lc,
                        ready = ready_symbol_extraction,
                        cooldown = symbol_extraction_ready_delay,
                        lock = lock,
                        stats = stats_for_symbol,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_symbol));
                    let stats = Arc::clone(&stats_for_symbol);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        crate::cron::symbol_extraction::run_symbol_extraction(db.as_ref(), &stats)
                            .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "symbol-extraction",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // SOTA Phase 1 — Function metrics cron (CC / Cognitive / Halstead / NPath / MI per function).
    // Sequenced after symbol-extraction (depends on file_symbols rows).
    let db_clone_fnmet = Arc::clone(&db);
    let rt_clone_fnmet = rt.clone();
    let stats_for_fnmet = Arc::clone(&stats);
    let function_metrics_interval = config.function_metrics_interval_secs;
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_fnmet: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let fnmet_ready_delay = Duration::from_secs(config.ready_delay_function_metrics_secs);
    let cron_pool_fnmet = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("function-metrics", function_metrics_interval * 1000),
        function_metrics_interval * 1000,
        "function-metrics",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_fnmet = Arc::clone(&ready_fnmet);
            let stats_for_fnmet = Arc::clone(&stats_for_fnmet);
            let db = db_clone_fnmet.clone();
            let rt = rt_clone_fnmet.clone();
            cron_pool_fnmet.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "function-metrics",
                        lc = lc,
                        ready = ready_fnmet,
                        cooldown = fnmet_ready_delay,
                        lock = lock,
                        stats = stats_for_fnmet,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_fnmet));
                    let stats = Arc::clone(&stats_for_fnmet);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        crate::cron::function_metrics::run_function_metrics(db.as_ref(), &stats)
                            .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "function-metrics",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // SOTA Phase 1 — Call-graph cron (symbol-resolved edges + fan_in/fan_out).
    // Sequenced after function-metrics (which depends on symbol-extraction's
    // file_symbols rows and seeds function_metrics rows for the fan_in/fan_out
    // UPDATE this cron issues).
    let db_clone_cg = Arc::clone(&db);
    let rt_clone_cg = rt.clone();
    let stats_for_cg = Arc::clone(&stats);
    let call_graph_interval = config.call_graph_interval_secs;
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_cg: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let cg_ready_delay = Duration::from_secs(config.ready_delay_call_graph_secs);
    let cron_pool_cg = Arc::clone(&cron_pool);
    // Same WorkPool the file-graph cron uses for parallel Brandes betweenness;
    // the call-graph cron now runs betweenness over the function call graph too.
    let cg_general_pool = general_pool.clone();
    handle.schedule_recurring(
        staggered_initial_delay_ms("call-graph", call_graph_interval * 1000),
        call_graph_interval * 1000,
        "call-graph",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_cg = Arc::clone(&ready_cg);
            let stats_for_cg = Arc::clone(&stats_for_cg);
            let db = db_clone_cg.clone();
            let rt = rt_clone_cg.clone();
            let wp = cg_general_pool.clone();
            cron_pool_cg.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "call-graph",
                        lc = lc,
                        ready = ready_cg,
                        cooldown = cg_ready_delay,
                        lock = lock,
                        stats = stats_for_cg,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_cg));
                    let stats = Arc::clone(&stats_for_cg);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        crate::cron::call_graph::run_call_graph(db.as_ref(), &stats, wp).await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "call-graph",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // RAPTOR-over-code summary tree (heavy — CUDA FCM per project; gates on
    // Ready + heavy_cron_lock). Sequenced (via ready-delay) after topic-
    // clustering so embeddings are settled before the conceptual tree is built.
    let db_clone_raptor = Arc::clone(&db);
    let rt_clone_raptor = rt.clone();
    let stats_for_raptor = Arc::clone(&stats);
    let raptor_interval = config.code_raptor_interval_secs;
    let lc = lifecycle.clone();
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_raptor: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let raptor_ready_delay = Duration::from_secs(config.ready_delay_code_raptor_secs);
    let cron_pool_raptor = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("code-raptor", raptor_interval * 1000),
        raptor_interval * 1000,
        "code-raptor",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_raptor = Arc::clone(&ready_raptor);
            let stats_for_raptor = Arc::clone(&stats_for_raptor);
            let db = db_clone_raptor.clone();
            let rt = rt_clone_raptor.clone();
            cron_pool_raptor.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "code-raptor",
                        lc = lc,
                        ready = ready_raptor,
                        cooldown = raptor_ready_delay,
                        lock = lock,
                        stats = stats_for_raptor,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_raptor));
                    let stats = Arc::clone(&stats_for_raptor);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    let lc_inner = lc.clone();
                    rt.block_on(async {
                        crate::cron::code_raptor::run_code_raptor(db.as_ref(), &stats, &lc_inner)
                            .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "code-raptor",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // Fuzzy-index sync — clone db / rt / stats BEFORE topic-clustering
    // claims the final-move ownership of each.
    let db_clone_fuzzy = Arc::clone(&db);
    let rt_clone_fuzzy = rt.clone();
    let stats_for_fuzzy = Arc::clone(&stats);
    let fuzzy_data_dir = fuzzy_config.data_dir.clone();
    let fuzzy_max_disk_bytes = fuzzy_config.max_disk_bytes;
    let fuzzy_eviction_cfg = fuzzy_config.eviction_config();
    let fuzzy_interval = config.fuzzy_sync_interval_secs;
    let cron_pool_fuzzy = Arc::clone(&cron_pool);
    let lc_fuzzy = lifecycle.clone();
    let lock_fuzzy = Arc::clone(&heavy_cron_lock);
    let ready_fuzzy: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let fuzzy_ready_delay = Duration::from_secs(config.ready_delay_topic_secs);
    handle.schedule_recurring(
        staggered_initial_delay_ms("fuzzy-sync", fuzzy_interval * 1000),
        fuzzy_interval * 1000,
        "fuzzy-sync",
        move || {
            if lc_fuzzy.is_stopping() {
                return false;
            }
            let lc = lc_fuzzy.clone();
            let lock = Arc::clone(&lock_fuzzy);
            let ready = Arc::clone(&ready_fuzzy);
            let stats = Arc::clone(&stats_for_fuzzy);
            let db = db_clone_fuzzy.clone();
            let rt = rt_clone_fuzzy.clone();
            let data_dir = fuzzy_data_dir.clone();
            let max_disk_bytes = fuzzy_max_disk_bytes;
            let eviction_cfg = fuzzy_eviction_cfg.clone();
            cron_pool_fuzzy.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "fuzzy-sync",
                        lc = lc,
                        ready = ready,
                        cooldown = fuzzy_ready_delay,
                        lock = lock,
                        stats = stats,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                    // Capture RSS + thread count around the run. `threads_delta`
                    // is the regression signal for the persistent-trie
                    // daemon-thread leak: a healthy run returns to ~0; a
                    // steadily-climbing delta means handles aren't being
                    // reclaimed.
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let threads_start = crate::stats::rss::current_thread_count().unwrap_or(0);
                    let t0 = Instant::now();
                    if let Some(pool) = db.pool().cloned() {
                        let stats_ref = Arc::clone(&stats);
                        let result = rt.block_on(async move {
                            crate::cron::fuzzy_sync::run_fuzzy_sync(
                                &pool,
                                &data_dir,
                                max_disk_bytes,
                                eviction_cfg,
                                stats_ref,
                            )
                            .await
                        });
                        let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                        let threads_end = crate::stats::rss::current_thread_count().unwrap_or(0);
                        match result {
                            Ok(report) => tracing::info!(
                                job = "fuzzy-sync",
                                symbols = report.symbols_synced,
                                paths = report.paths_synced,
                                commits = report.commits_synced,
                                durable_mandates = report.durable_mandates_synced,
                                rss_mb_start = rss_start >> 20,
                                rss_mb_end = rss_end >> 20,
                                rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                                threads_start,
                                threads_end,
                                threads_delta = threads_end as i64 - threads_start as i64,
                                elapsed_s = t0.elapsed().as_secs_f64(),
                                "fuzzy-sync run complete"
                            ),
                            Err(e) => tracing::error!(
                                job = "fuzzy-sync",
                                error = %e,
                                "fuzzy-sync run failed"
                            ),
                        }
                    } else {
                        tracing::warn!(job = "fuzzy-sync", "skipping run: DbClient has no pool");
                    }
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );

    // BGE-M3 embedding backfill (off when interval = 0). The cron
    // drains `file_chunks` + `session_prompts` rows whose
    // `embedding_v2` (1024d) column is NULL, embeds the source text
    // with BGE-M3, and writes back the new column plus
    // `embedding_signature = 'bge-m3-v1'`. pgmcp is BGE-M3/1024-only
    // (ADR-005): the schema is pinned to `bge-m3-v1` at migration time,
    // so there is no separate cutover step — this cron just fills any
    // 1024d columns still left NULL.
    // quality-history (heavy — snapshots each project's quality GPAs into
    // `quality_report_history` so the trend/forecast tools + digest read a
    // trajectory, not a single point; it fans out the quality collectors via
    // `quality::aggregate`). Interval-gated like embedding-migration.
    if config.quality_history_interval_secs > 0 {
        let rt_clone_qh = rt.clone();
        let stats_for_qh = Arc::clone(&stats);
        let qh_interval = config.quality_history_interval_secs;
        let cron_pool_qh = Arc::clone(&cron_pool);
        let lc_qh = lifecycle.clone();
        // Own lock (not heavy_cron_lock) so the cheap snapshot stops starving
        // behind multi-minute GPU crons. See `quality_history_lock` above.
        let lock_qh = Arc::clone(&quality_history_lock);
        let ready_qh: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
        let qh_ready_delay = Duration::from_secs(120);
        let ctx_qh = system_ctx.clone();
        handle.schedule_recurring(
            staggered_initial_delay_ms("quality-history", qh_interval * 1000),
            qh_interval * 1000,
            "quality-history",
            move || {
                if lc_qh.is_stopping() {
                    return false;
                }
                let lc = lc_qh.clone();
                let lock = Arc::clone(&lock_qh);
                let ready = Arc::clone(&ready_qh);
                let stats = Arc::clone(&stats_for_qh);
                let rt = rt_clone_qh.clone();
                let ctx = ctx_qh.clone();
                cron_pool_qh.submit(
                    move || {
                        let _guard = heavy_gate_or_skip!(
                            job = "quality-history",
                            lc = lc,
                            ready = ready,
                            cooldown = qh_ready_delay,
                            lock = lock,
                            stats = stats,
                        );
                        let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                        let stats_run = Arc::clone(&stats);
                        rt.block_on(async move {
                            crate::cron::quality_history::run_or_log(ctx, stats_run).await;
                        });
                    },
                    crate::work_pool::pool::Priority::Low,
                );
                true
            },
        );
    }

    if config.embedding_migration_interval_secs > 0 {
        let db_clone_mig = Arc::clone(&db);
        let rt_clone_mig = rt.clone();
        let stats_for_mig = Arc::clone(&stats);
        let mig_interval = config.embedding_migration_interval_secs;
        let mig_cfg = crate::cron::embedding_migration::EmbeddingMigrationConfig::new(
            embeddings_config.clone(),
            config.embedding_migration_batch_size,
            config.embedding_migration_max_batches,
        );
        let cron_pool_mig = Arc::clone(&cron_pool);
        let lc_mig = lifecycle.clone();
        let lock_mig = Arc::clone(&heavy_cron_lock);
        let ready_mig: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
        // F6 (boy-scout 2026-05-25): use the migration-specific
        // ready-delay (default 60s) instead of reusing
        // `ready_delay_topic_secs` (default 3600s). Migration has
        // nothing to wait for post-Ready — it just drains rows
        // whose `embedding_v2` column is NULL. The prior 1-hour
        // delay blocked the BGE-M3 cutover drain for an hour after
        // every daemon restart.
        let mig_ready_delay = Duration::from_secs(config.ready_delay_embedding_migration_secs);
        handle.schedule_recurring(
            staggered_initial_delay_ms("embedding-migration", mig_interval * 1000),
            mig_interval * 1000,
            "embedding-migration",
            move || {
                if lc_mig.is_stopping() {
                    return false;
                }
                let lc = lc_mig.clone();
                let lock = Arc::clone(&lock_mig);
                let ready = Arc::clone(&ready_mig);
                let stats = Arc::clone(&stats_for_mig);
                let db = db_clone_mig.clone();
                let rt = rt_clone_mig.clone();
                let mig_cfg = mig_cfg.clone();
                cron_pool_mig.submit(
                    move || {
                        let _guard = heavy_gate_or_skip!(
                            job = "embedding-migration",
                            lc = lc,
                            ready = ready,
                            cooldown = mig_ready_delay,
                            lock = lock,
                            stats = stats,
                        );
                        let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                        if let Some(pool) = db.pool().cloned() {
                            let stats_ref = Arc::clone(&stats);
                            rt.block_on(async move {
                                crate::cron::embedding_migration::run_or_log(
                                    Arc::new(pool),
                                    stats_ref,
                                    mig_cfg,
                                )
                                .await;
                            });
                        } else {
                            tracing::warn!(
                                job = "embedding-migration",
                                "skipping run: DbClient has no pool"
                            );
                        }
                    },
                    crate::work_pool::pool::Priority::Low,
                );
                true
            },
        );
    }

    // ngram-lm-train cron — per-project HybridLM training (n-gram +
    // subword embedding) used by the third RRF leg of
    // `tool_hybrid_search` and by `tool_correct_query`. Off when
    // interval = 0.
    if config.ngram_lm_train_interval_secs > 0 {
        let db_clone_lm = Arc::clone(&db);
        let rt_clone_lm = rt.clone();
        let stats_for_lm = Arc::clone(&stats);
        let lm_interval = config.ngram_lm_train_interval_secs;
        let lm_data_dir = fuzzy_config.data_dir.clone();
        let cron_pool_lm = Arc::clone(&cron_pool);
        let lc_lm = lifecycle.clone();
        let lock_lm = Arc::clone(&heavy_cron_lock);
        let ready_lm: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
        let lm_ready_delay = Duration::from_secs(config.ready_delay_topic_secs);
        handle.schedule_recurring(
            staggered_initial_delay_ms("ngram-lm-train", lm_interval * 1000),
            lm_interval * 1000,
            "ngram-lm-train",
            move || {
                if lc_lm.is_stopping() {
                    return false;
                }
                let lc = lc_lm.clone();
                let lock = Arc::clone(&lock_lm);
                let ready = Arc::clone(&ready_lm);
                let stats = Arc::clone(&stats_for_lm);
                let db = db_clone_lm.clone();
                let rt = rt_clone_lm.clone();
                let data_dir = lm_data_dir.clone();
                cron_pool_lm.submit(
                    move || {
                        let _guard = heavy_gate_or_skip!(
                            job = "ngram-lm-train",
                            lc = lc,
                            ready = ready,
                            cooldown = lm_ready_delay,
                            lock = lock,
                            stats = stats,
                        );
                        let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                        let t0 = Instant::now();
                        if let Some(pool) = db.pool().cloned() {
                            let stats_ref = Arc::clone(&stats);
                            rt.block_on(async move {
                                crate::cron::ngram_lm_train::run_or_log(
                                    Arc::new(pool),
                                    stats_ref,
                                    data_dir,
                                )
                                .await;
                            });
                            tracing::info!(
                                job = "ngram-lm-train",
                                elapsed_s = t0.elapsed().as_secs_f64(),
                                "ngram-lm-train run complete"
                            );
                        } else {
                            tracing::warn!(
                                job = "ngram-lm-train",
                                "skipping run: DbClient has no pool"
                            );
                        }
                    },
                    crate::work_pool::pool::Priority::Low,
                );
                true
            },
        );
    }

    // Topic-dendrogram cron — hierarchical-agglomerative + c-TF-IDF
    // built on top of the same chunks the online FCM owns. Persists
    // to `topic_dendrograms`; the `dendrogram_topic_hierarchy` MCP
    // tool reads from there. Off when interval = 0.
    if config.topic_dendrogram_interval_secs > 0 {
        let db_clone_td = Arc::clone(&db);
        let rt_clone_td = rt.clone();
        let stats_for_td = Arc::clone(&stats);
        let td_interval = config.topic_dendrogram_interval_secs;
        let cron_pool_td = Arc::clone(&cron_pool);
        let lc_td = lifecycle.clone();
        let lock_td = Arc::clone(&heavy_cron_lock);
        let ready_td: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
        let td_ready_delay = Duration::from_secs(config.ready_delay_topic_secs);
        handle.schedule_recurring(
            staggered_initial_delay_ms("topic-dendrogram", td_interval * 1000),
            td_interval * 1000,
            "topic-dendrogram",
            move || {
                if lc_td.is_stopping() {
                    return false;
                }
                let lc = lc_td.clone();
                let lock = Arc::clone(&lock_td);
                let ready = Arc::clone(&ready_td);
                let stats = Arc::clone(&stats_for_td);
                let db = db_clone_td.clone();
                let rt = rt_clone_td.clone();
                cron_pool_td.submit(
                    move || {
                        let _guard = heavy_gate_or_skip!(
                            job = "topic-dendrogram",
                            lc = lc,
                            ready = ready,
                            cooldown = td_ready_delay,
                            lock = lock,
                            stats = stats,
                        );
                        let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                        let t0 = Instant::now();
                        if let Some(pool) = db.pool().cloned() {
                            let stats_ref = Arc::clone(&stats);
                            rt.block_on(async move {
                                crate::cron::topic_dendrogram::run_or_log(
                                    Arc::new(pool),
                                    stats_ref,
                                )
                                .await;
                            });
                            tracing::info!(
                                job = "topic-dendrogram",
                                elapsed_s = t0.elapsed().as_secs_f64(),
                                "topic-dendrogram run complete"
                            );
                        } else {
                            tracing::warn!(
                                job = "topic-dendrogram",
                                "skipping run: DbClient has no pool"
                            );
                        }
                    },
                    crate::work_pool::pool::Priority::Low,
                );
                true
            },
        );
    }

    // Topic clustering (global full-chunk — always produces scope = "global")
    let db_clone_topic = db; // final move // final move
    let rt_clone_topic = rt; // final move
    let stats_for_topic = stats; // final move
    let topic_interval = config.topic_scan_interval_secs;
    let lc = lifecycle; // final move
    let lock = Arc::clone(&heavy_cron_lock);
    let ready_topic: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let topic_ready_delay = Duration::from_secs(config.ready_delay_topic_secs);
    let topic_cron_config = CronConfig {
        topic_scan_interval_secs: topic_interval,
        topic_min_cluster_size: config.topic_min_cluster_size,
        topic_num_clusters: config.topic_num_clusters,
        topic_fuzziness: config.topic_fuzziness,
        topic_fcm_max_iters: config.topic_fcm_max_iters,
        topic_fcm_tolerance: config.topic_fcm_tolerance,
        topic_membership_threshold: config.topic_membership_threshold,
        topic_label_top_k: config.topic_label_top_k,
        topic_max_mem_fraction: config.topic_max_mem_fraction,
        topic_scratch_dir: config.topic_scratch_dir.clone(),
        ..CronConfig::default()
    };
    let cron_pool_topic = Arc::clone(&cron_pool);
    handle.schedule_recurring(
        staggered_initial_delay_ms("topic-clustering", topic_interval * 1000),
        topic_interval * 1000,
        "topic-clustering",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let ready_topic = Arc::clone(&ready_topic);
            let stats_for_topic = Arc::clone(&stats_for_topic);
            let db = db_clone_topic.clone();
            let cfg = topic_cron_config.clone();
            let rt = rt_clone_topic.clone();
            cron_pool_topic.submit(
                move || {
                    let _guard = heavy_gate_or_skip!(
                        job = "topic-clustering",
                        lc = lc,
                        ready = ready_topic,
                        cooldown = topic_ready_delay,
                        lock = lock,
                        stats = stats_for_topic,
                    );
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_topic));
                    let stats = Arc::clone(&stats_for_topic);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    let lc_inner = lc.clone();
                    rt.block_on(async {
                        crate::cron::topic_clustering::run_global_topic_scan(
                            db.as_ref(),
                            &cfg,
                            &stats,
                            &lc_inner,
                        )
                        .await;
                    });
                    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    tracing::info!(
                        job = "topic-clustering",
                        rss_mb_start = rss_start >> 20,
                        rss_mb_end = rss_end >> 20,
                        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                        elapsed_s = t0.elapsed().as_secs_f64(),
                        "heavy cron complete"
                    );
                },
                crate::work_pool::pool::Priority::Low,
            );
            true
        },
    );
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn stagger_is_deterministic_per_job_name() {
        // The same job name must produce the same offset across calls so
        // a daemon restart doesn't re-shuffle every cron's schedule on each
        // restart. Operators rely on stable cadence for capacity planning.
        let one = staggered_initial_delay_ms("similarity-scan", 21_600_000);
        let two = staggered_initial_delay_ms("similarity-scan", 21_600_000);
        assert_eq!(one, two);
    }

    #[test]
    fn stagger_distinct_names_yield_distinct_offsets() {
        // The whole point of the stagger is breaking the synchronized-tick
        // collision that caused the 19:11–19:25 pool starvation in the
        // 2026-05-21 pgmcp.log. If two heavy crons hashed to the same
        // offset, they'd re-collide at every interval boundary.
        let a = staggered_initial_delay_ms("similarity-scan", 21_600_000);
        let b = staggered_initial_delay_ms("graph-analysis", 7_200_000);
        let c = staggered_initial_delay_ms("symbol-extraction", 7_200_000);
        let d = staggered_initial_delay_ms("call-graph", 7_200_000);
        let e = staggered_initial_delay_ms("topic-clustering", 43_200_000);
        let f = staggered_initial_delay_ms("function-metrics", 7_200_000);
        let g = staggered_initial_delay_ms("git-history-index", 3_600_000);
        let mut all = [a, b, c, d, e, f, g];
        all.sort_unstable();
        for pair in all.windows(2) {
            assert_ne!(pair[0], pair[1], "offsets collided: {all:?}");
        }
    }

    #[test]
    fn stagger_respects_jitter_cap_and_base() {
        // For very fast crons (60s interval) the cap is interval/2 = 30_000ms.
        // For long crons (≥ 20 min interval) the cap is the 10-min ceiling.
        // In both cases the offset must be ≥ BASE_MS (1s) so we still
        // give the daemon a startup grace period.
        for name in ["job-a", "job-b", "job-c", "another-name", "z"] {
            let fast = staggered_initial_delay_ms(name, 60_000);
            assert!((1_000..=31_000).contains(&fast), "fast={fast}");
            let slow = staggered_initial_delay_ms(name, 21_600_000);
            assert!((1_000..=601_000).contains(&slow), "slow={slow}");
        }
    }

    #[test]
    fn stagger_handles_zero_and_one_interval() {
        // Edge case: interval_ms = 0 would divide by zero without the
        // `.max(1)` clamp in the helper. interval_ms = 1 stresses the
        // floor of the cap. Both must return BASE_MS exactly because the
        // hash modulo 1 is always 0.
        assert_eq!(staggered_initial_delay_ms("x", 0), 1_000);
        assert_eq!(staggered_initial_delay_ms("x", 1), 1_000);
    }

    #[test]
    fn test_state_transitions() {
        let (_, rx) = unbounded::<ScheduledTask>();
        let terminating = Arc::new(AtomicBool::new(false));
        let sm = CronStateMachine::new(rx, terminating, 100, None, None);
        assert_eq!(sm.current_state(), CronState::CheckEvents);
    }

    #[test]
    fn test_termination_from_any_state() {
        let (_, rx) = unbounded::<ScheduledTask>();
        let terminating = Arc::new(AtomicBool::new(false));
        let mut sm = CronStateMachine::new(rx, terminating.clone(), 100, None, None);
        terminating.store(true, AtomicOrdering::Release);
        sm.run();
        assert_eq!(sm.current_state(), CronState::Terminated);
    }

    #[test]
    fn test_concurrent_task_submission() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, _ready) = spawn_cron(Arc::clone(&terminating), None);
        let counter = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let h = handle.clone();
                let c = Arc::clone(&counter);
                thread::spawn(move || {
                    for _ in 0..100 {
                        h.schedule_after(0, TaskMetadata::OneShot, {
                            let c = Arc::clone(&c);
                            move || {
                                c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                true
                            }
                        });
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("Thread panicked");
        }

        thread::sleep(Duration::from_millis(500));
        handle.request_shutdown();
        thread.join().expect("Cron thread panicked");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1000);
    }

    #[test]
    fn test_recurring_task() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, _ready) = spawn_cron_with_interval(Arc::clone(&terminating), 10, None);
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        handle.schedule_recurring(0, 50, "counter", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        thread::sleep(Duration::from_millis(275));
        handle.request_shutdown();
        thread.join().expect("Cron thread panicked");
        let count = counter.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            (4..=7).contains(&count),
            "Expected 4-7 executions, got {}",
            count
        );
    }

    #[test]
    fn test_one_shot_task() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, _ready) = spawn_cron_with_interval(Arc::clone(&terminating), 10, None);
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        handle.schedule_once(0, "one-shot", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        thread::sleep(Duration::from_millis(100));
        handle.request_shutdown();
        thread.join().expect("Cron thread panicked");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_panic_safety() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, ready_rx) =
            spawn_cron_with_interval(Arc::clone(&terminating), 10, None);
        ready_rx.recv().expect("Cron thread failed to start");

        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        // Synchronous one-shot signal: the normal task pushes through
        // this channel the instant it runs, and the test thread blocks
        // on `recv_timeout` until then. This asserts the actual
        // panic-safety property (the post-panic task runs) without
        // baking in a tight wall-clock budget — a contested host can
        // take a few hundred ms longer than a quiet one, and that's
        // fine; what matters is that the cron *did* recover.
        //
        // The `Mutex<Option<Sender>>` is so the closure can stay
        // `FnMut + 'static` (what `schedule_once` requires) while still
        // consuming the sender exactly once via `take()`.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let done_tx = std::sync::Mutex::new(Some(done_tx));

        handle.schedule_once(0, "panicking", || {
            panic!("intentional panic");
        });
        handle.schedule_once(50, "normal", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(tx) = done_tx.lock().expect("done_tx mutex").take() {
                let _ = tx.send(());
            }
            true
        });

        // 30 s is generous on purpose: the green path completes in
        // ~50–150 ms; anything approaching 30 s indicates a genuine
        // panic-safety regression (the cron wedged after the prior
        // task panicked), not a flake.
        done_rx.recv_timeout(Duration::from_secs(30)).expect(
            "Cron thread did not process the post-panic task within 30s — \
             panic-safety regression: the cron is wedged after the prior task panicked.",
        );

        handle.request_shutdown();
        thread
            .join()
            .expect("Cron thread should not panic from task panic");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_task_ordering() {
        let mut heap = BinaryHeap::new();
        heap.push(ScheduledTask {
            scheduled_time_ms: 300,
            metadata: TaskMetadata::OneShot,
            task: Box::new(|| true),
        });
        heap.push(ScheduledTask {
            scheduled_time_ms: 100,
            metadata: TaskMetadata::OneShot,
            task: Box::new(|| true),
        });
        heap.push(ScheduledTask {
            scheduled_time_ms: 200,
            metadata: TaskMetadata::OneShot,
            task: Box::new(|| true),
        });
        assert_eq!(heap.pop().expect("should have task").scheduled_time_ms, 100);
        assert_eq!(heap.pop().expect("should have task").scheduled_time_ms, 200);
        assert_eq!(heap.pop().expect("should have task").scheduled_time_ms, 300);
    }

    #[test]
    fn test_shutdown_flag() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, _ready) = spawn_cron(Arc::clone(&terminating), None);
        assert!(!handle.is_shutting_down());
        handle.request_shutdown();
        assert!(handle.is_shutting_down());
        thread.join().expect("Cron thread panicked");
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    proptest! {
        #![proptest_config(ProptestConfig { cases: 4, ..ProptestConfig::default() })]

        /// For any interleaving of one-shot task submissions across
        /// multiple producer threads, every submitted task runs to
        /// completion exactly once.
        #[test]
        fn prop_arbitrary_task_interleavings_all_complete(
            producers in 2usize..4,
            per_producer in 3usize..8,
        ) {
            let terminating = Arc::new(AtomicBool::new(false));
            let (handle, thread, ready) = spawn_cron(Arc::clone(&terminating), None);
            ready.recv().expect("cron ready");

            let counter = Arc::new(AtomicUsize::new(0));
            let mut producer_handles = Vec::new();
            for _ in 0..producers {
                let handle = handle.clone();
                let counter = Arc::clone(&counter);
                producer_handles.push(std::thread::spawn(move || {
                    for idx in 0..per_producer {
                        let c = Arc::clone(&counter);
                        handle.schedule_once(0, &format!("soak_{}", idx), move || {
                            c.fetch_add(1, Ordering::Relaxed);
                            false
                        });
                    }
                }));
            }
            for ph in producer_handles {
                ph.join().expect("producer");
            }

            // Wait up to 5s for all tasks to drain.
            let expected = producers * per_producer;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while counter.load(Ordering::Relaxed) < expected
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let final_count = counter.load(Ordering::Relaxed);
            handle.request_shutdown();
            thread.join().expect("cron thread");
            prop_assert_eq!(final_count, expected,
                "lost {} of {} submitted tasks", expected - final_count, expected);
        }
    }

    // F3: `heavy_gate_or_skip!` shutdown gate. Plan reference:
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md F3.

    use crate::daemon_state::{DaemonLifecycle, DaemonPhase};
    use crate::stats::tracker::{
        CronJobOutcome as TestCronJobOutcome, SkipReason as TestSkipReason, StatsTracker,
    };
    use std::sync::OnceLock as TestOnceLock;

    /// Helper: invoke the body of `heavy_gate_or_skip!` inside a
    /// `()`-returning closure (the macro early-returns with bare
    /// `return;`, so the closure cannot return a value). The shared
    /// `Arc<AtomicBool>` is flipped to `true` only if the macro
    /// passes through every gate. Caller reads the flag to determine
    /// "would the work body have run?".
    fn try_gate(lc: &DaemonLifecycle, stats: &Arc<StatsTracker>) -> bool {
        // Pre-warm cooldown deadline so cooldown can't be the
        // returning reason; we want to isolate the shutdown gate.
        let ready: Arc<TestOnceLock<Instant>> = Arc::new(TestOnceLock::new());
        let _ = ready.set(Instant::now() - Duration::from_secs(3600));
        let lock = Arc::new(parking_lot::Mutex::new(()));
        let proceeded = Arc::new(AtomicBool::new(false));

        let proceeded_for_closure = Arc::clone(&proceeded);
        let inner = || {
            // The macro requires a string-literal job name (matched
            // by `$job:literal`). Tests each construct their own
            // `StatsTracker`, so re-using one literal across the
            // three tests does not race on the DashMap.
            let _guard = heavy_gate_or_skip!(
                job = "heavy-gate-shutdown-test",
                lc = lc,
                ready = ready,
                cooldown = Duration::from_secs(0),
                lock = lock,
                stats = stats,
            );
            proceeded_for_closure.store(true, AtomicOrdering::Relaxed);
        };
        inner();
        proceeded.load(AtomicOrdering::Relaxed)
    }

    #[test]
    fn heavy_gate_returns_phase_gate_when_not_ready() {
        let lc = DaemonLifecycle::new();
        let stats = Arc::new(StatsTracker::new());
        // Phase: Initializing < Ready → PhaseGate fires.
        let proceeded = try_gate(&lc, &stats);
        assert!(!proceeded, "PhaseGate must short-circuit when not Ready");
        let recorded = stats
            .last_cron_outcomes
            .get("heavy-gate-shutdown-test")
            .expect("outcome recorded")
            .outcome;
        assert_eq!(
            recorded,
            TestCronJobOutcome::Skipped(TestSkipReason::PhaseGate),
            "expected PhaseGate, got {:?}",
            recorded,
        );
    }

    #[test]
    fn heavy_gate_returns_shutdown_when_terminating() {
        let lc = DaemonLifecycle::new();
        let stats = Arc::new(StatsTracker::new());
        // Transition through Scanning → Ready → Terminating. Ready is
        // crossed so the PhaseGate test passes; Terminating > Ready
        // numerically so `is_at_least(Ready)` returns true and only
        // the explicit `is_stopping()` check can catch it.
        lc.transition(DaemonPhase::Scanning);
        lc.transition(DaemonPhase::Ready);
        lc.transition(DaemonPhase::Terminating);
        assert!(lc.is_stopping(), "precondition: lifecycle reports stopping");
        assert!(
            lc.is_at_least(DaemonPhase::Ready),
            "precondition: Terminating is_at_least(Ready)"
        );

        let proceeded = try_gate(&lc, &stats);
        assert!(
            !proceeded,
            "Shutdown gate must short-circuit during Terminating"
        );
        let recorded = stats
            .last_cron_outcomes
            .get("heavy-gate-shutdown-test")
            .expect("outcome recorded")
            .outcome;
        assert_eq!(
            recorded,
            TestCronJobOutcome::Skipped(TestSkipReason::Shutdown),
            "expected Shutdown, got {:?}",
            recorded,
        );
    }

    #[test]
    fn heavy_gate_proceeds_when_ready_and_not_stopping() {
        let lc = DaemonLifecycle::new();
        let stats = Arc::new(StatsTracker::new());
        lc.transition(DaemonPhase::Scanning);
        lc.transition(DaemonPhase::Ready);
        let proceeded = try_gate(&lc, &stats);
        assert!(proceeded, "no gate should fire when Ready and not stopping");
        // No outcome recorded by the gate macros on success — the work
        // body would record one. Verify by confirming no entry was
        // inserted by our test's macro invocation.
        assert!(
            stats
                .last_cron_outcomes
                .get("heavy-gate-shutdown-test")
                .is_none(),
            "no skip outcome should be recorded when the gate passes"
        );
    }
}
