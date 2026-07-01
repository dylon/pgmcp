//! Generic Lock-Free Reactive State Machine Task Scheduler
//!
//! Adapted from MeTTaTron's task_scheduler.rs.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use tracing::error;

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

/// Restart-survival initial delay (ADR-018 §5). Computes a recurring cron's
/// first-tick delay from its last persisted successful completion instead of a
/// fresh stagger, so a daemon restart no longer slips the schedule. Pure and
/// unit-testable (`now_ms` injected). Overdue / unknown-job / clock-skew all
/// fall back to the anti-herd [`staggered_initial_delay_ms`]; for short-interval
/// jobs the result naturally collapses to ~stagger (since `next_due ≈ now`),
/// which is correct, not a special case.
pub(crate) fn restart_initial_delay_ms(
    job: &str,
    interval_ms: u64,
    last_ok_ms: Option<u64>,
    now_ms: u64,
) -> u64 {
    if interval_ms == 0 {
        return 0; // disabled: caller won't schedule
    }
    let stagger = staggered_initial_delay_ms(job, interval_ms);
    match last_ok_ms {
        None => stagger,                        // first boot / unknown job
        Some(last) if last > now_ms => stagger, // clock skew / future timestamp
        Some(last) => {
            let next_due = last.saturating_add(interval_ms);
            if next_due <= now_ms {
                stagger // overdue → fire soon, but staggered (anti-herd)
            } else {
                (next_due - now_ms).max(stagger) // wait remaining, floor at stagger
            }
        }
    }
}

/// RAII guard that flips `stats.heavy_cron_running` → true on construction
/// and back to false on drop. Used by the four heavy cron bodies so the
/// Prometheus `pgmcp_heavy_cron_running` gauge reflects live state regardless
/// of early-return or panic.
pub(crate) struct HeavyCronFlag {
    stats: Arc<StatsTracker>,
}

impl HeavyCronFlag {
    pub(crate) fn new(stats: Arc<StatsTracker>) -> Self {
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

/// Heavy-cron skip-gate (ADR-018 §6). Returns the held `heavy_cron_lock`
/// `MutexGuard` when the body may proceed, or `None` after recording the exact
/// `SkipReason` into both `stats.last_cron_outcomes` (the in-memory
/// `index_stats` snapshot) and the durable `cron_run_history` ledger (via
/// `hist.record_skip`) so an operator can tell which gate is silencing each cron
/// — live and historically.
///
/// Without recording, the heavy-cron closures silently `return;` at each gate
/// and the scheduler records `CronJobOutcome::Ok` (because the *closure*
/// returned cleanly). Bug C in the 2026-05-21 staleness investigation: 363
/// cron_executions with zero work-counters because every tick hit a gate.
///
/// The **Cooldown** (ready-relative settle) branch deliberately lives *outside*
/// this gate now — it moved up into the recurring closure (`heavy_cron_tick`)
/// so a settle-skip can schedule a one-shot retry at cooldown-expiry instead of
/// slipping the run by a full interval. This gate keeps the checks that must be
/// evaluated *inside* the pool task: PhaseGate, Shutdown, DbDown, DiskPressure,
/// LockBusy.
fn try_heavy_gate<'a>(
    job: &'static str,
    lc: &DaemonLifecycle,
    lock: &'a tokio::sync::Mutex<()>,
    stats: &StatsTracker,
    hist: &crate::cron::history::CronHistoryWriter,
) -> Option<tokio::sync::MutexGuard<'a, ()>> {
    use crate::cron::history::CronTriggerSource;
    use crate::stats::tracker::{CronJobOutcome, SkipReason};
    if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
        stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::PhaseGate), 0);
        hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::PhaseGate);
        return None;
    }
    // The PhaseGate check passes through `Terminating` because `DaemonPhase` is
    // ordered Initializing < Scanning < Ready < Terminating < Defunct. Without
    // this second gate, closures already enqueued at SIGTERM race the closing PG
    // pool / channels and the next pool-acquire logs "attempted to acquire a
    // connection on a closed pool" (plan glittery-graham F3).
    if lc.is_stopping() {
        stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::Shutdown), 0);
        hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::Shutdown);
        return None;
    }
    // DB-availability breaker (src/health): if the database is unreachable, skip
    // quietly rather than stall on a 10 s `acquire_timeout` and log every tick.
    if !stats.db_health().is_up() {
        stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::DbDown), 0);
        hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::DbDown);
        return None;
    }
    // Disk-pressure gate (src/health): heavy crons write/grow data, so pause them
    // while the watchdog reports a watched filesystem under pressure.
    if stats.disk_pressure().is_paused() {
        stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::DiskPressure), 0);
        hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::DiskPressure);
        return None;
    }
    match lock.try_lock() {
        Ok(g) => Some(g),
        Err(_) => {
            tracing::info!(job, "heavy cron busy, deferring");
            stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::LockBusy), 0);
            hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::LockBusy);
            None
        }
    }
}

/// Slack added to a cooldown-retry one-shot so it fires just *past* the settle
/// window rather than on its exact edge (`heavy_cron_tick` / ADR-018 §6).
const COOLDOWN_SLACK_MS: u64 = 1_000;

/// The ready-relative settle decision for a heavy cron, evaluated on the
/// scheduler thread (not in the pool task) so a skip can be retried at
/// cooldown-expiry instead of slipping a full interval.
enum CooldownDecision {
    /// Submit the body now (the in-pool gates still apply).
    RunNow,
    /// Still inside the post-Ready settle window; retry after this many ms.
    DeferMs(u64),
}

/// Decide whether a heavy cron may submit now or must wait out the post-boot
/// settle. Measures **time in the Ready phase** (`ms_in_current_phase`) — a true
/// "N seconds after Ready" gate that survives across the body's own timing and
/// resets correctly if the daemon phase regresses (e.g. a mid-run reindex).
fn cooldown_decision(lc: &DaemonLifecycle, cooldown_ms: u64) -> CooldownDecision {
    if cooldown_ms == 0 {
        return CooldownDecision::RunNow;
    }
    // Not yet Ready (or already stopping): defer to the in-pool PhaseGate /
    // Shutdown gates, which record the precise skip reason. Submitting now is
    // exactly today's behavior (the outer closure never gated on Ready).
    if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) || lc.is_stopping() {
        return CooldownDecision::RunNow;
    }
    let since_ready = lc.ms_in_current_phase().max(0) as u64;
    if since_ready < cooldown_ms {
        CooldownDecision::DeferMs(cooldown_ms - since_ready + COOLDOWN_SLACK_MS)
    } else {
        CooldownDecision::RunNow
    }
}

/// Schedule a one-shot retry of a cooldown-deferred heavy cron at
/// `delay_ms`. The one-shot re-evaluates the settle on fire: if the window has
/// elapsed it runs the body; otherwise it re-arms itself. Bounded —
/// `ms_in_current_phase` grows monotonically while Ready, and a phase
/// regression simply re-waits Ready (ADR-018 §6).
fn schedule_heavy_retry(
    handle: CronHandle,
    lc: DaemonLifecycle,
    run_once: Arc<dyn Fn() + Send + Sync>,
    cooldown_ms: u64,
    job: &'static str,
    delay_ms: u64,
) {
    // Clone for the closure capture so `handle` itself stays free to be the
    // `schedule_once` receiver (the closure re-arms via `handle_inner`).
    let handle_inner = handle.clone();
    handle.schedule_once(delay_ms, job, move || {
        match cooldown_decision(&lc, cooldown_ms) {
            CooldownDecision::RunNow => run_once(),
            CooldownDecision::DeferMs(d) => schedule_heavy_retry(
                handle_inner.clone(),
                lc.clone(),
                Arc::clone(&run_once),
                cooldown_ms,
                job,
                d,
            ),
        }
        false // one-shot
    });
}

/// The recurring-closure body shared by every heavy cron (ADR-018 §6). On each
/// tick it either submits the body now (`run_once`) or, if still inside the
/// post-boot settle, records a `Cooldown` skip and schedules a one-shot retry at
/// expiry — instead of letting the skip slip the run a full interval. Returns
/// `true` to keep the recurring schedule (the retry is additive).
fn heavy_cron_tick(
    lc: &DaemonLifecycle,
    handle: &CronHandle,
    hist: &crate::cron::history::CronHistoryWriter,
    stats: &StatsTracker,
    job: &'static str,
    cooldown_ms: u64,
    run_once: &Arc<dyn Fn() + Send + Sync>,
) -> bool {
    use crate::cron::history::CronTriggerSource;
    use crate::stats::tracker::{CronJobOutcome, SkipReason};
    if lc.is_stopping() {
        return false;
    }
    match cooldown_decision(lc, cooldown_ms) {
        CooldownDecision::RunNow => run_once(),
        CooldownDecision::DeferMs(d) => {
            stats.record_cron_outcome(job, CronJobOutcome::Skipped(SkipReason::Cooldown), 0);
            hist.record_skip(job, CronTriggerSource::Scheduled, SkipReason::Cooldown);
            schedule_heavy_retry(
                handle.clone(),
                lc.clone(),
                Arc::clone(run_once),
                cooldown_ms,
                job,
                d,
            );
        }
    }
    true
}

/// Register one heavy cron with restart-survival scheduling (ADR-018 §5) and the
/// honor-settle cooldown (§6). `body` is the cron-specific work; it runs inside
/// the pool task behind [`try_heavy_gate`] + [`HeavyCronFlag`], receiving a
/// `&mut CronRunGuard` so it can record the run's outcome and counters. The
/// guard captures duration / RSS / thread deltas and persists exactly one
/// `cron_run_history` row on drop (`Panicked` if the body unwinds). `body` is
/// `Fn` because the recurring tick and each cooldown retry submit it afresh.
#[allow(clippy::too_many_arguments)]
fn register_heavy_cron<F>(
    handle: &CronHandle,
    lifecycle: &DaemonLifecycle,
    cron_pool: &Arc<crate::work_pool::pool::WorkPool>,
    heavy_cron_lock: &Arc<tokio::sync::Mutex<()>>,
    stats: &Arc<StatsTracker>,
    hist: &crate::cron::history::CronHistoryWriter,
    job: &'static str,
    initial_delay_ms: u64,
    interval_ms: u64,
    cooldown_ms: u64,
    body: F,
) where
    F: Fn(&mut crate::cron::history::CronRunGuard) + Send + Sync + 'static,
{
    let body = Arc::new(body);
    // Re-callable submit closure: the recurring tick calls it when the settle has
    // elapsed; the cooldown trampoline calls it again at expiry. Each call clones
    // the shared state and enqueues one fresh pool task.
    let run_once: Arc<dyn Fn() + Send + Sync> = {
        let cron_pool = Arc::clone(cron_pool);
        let lc = lifecycle.clone();
        let lock = Arc::clone(heavy_cron_lock);
        let stats = Arc::clone(stats);
        let hist = hist.clone();
        Arc::new(move || {
            let lc = lc.clone();
            let lock = Arc::clone(&lock);
            let stats = Arc::clone(&stats);
            let hist = hist.clone();
            let body = Arc::clone(&body);
            cron_pool.submit(
                move || {
                    let Some(_lock) = try_heavy_gate(job, &lc, &lock, &stats, &hist) else {
                        return;
                    };
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats));
                    let mut guard = crate::cron::history::CronRunGuard::new(
                        hist.clone(),
                        job,
                        crate::cron::history::CronTriggerSource::Scheduled,
                        None,
                    );
                    body(&mut guard);
                    // `guard` drops here → records one row (Ok/NoOp/Failed set by
                    // `body`, or Panicked if `body` unwound).
                },
                crate::work_pool::pool::Priority::Low,
            );
        })
    };
    let lc = lifecycle.clone();
    let handle_for_tick = handle.clone();
    let hist_for_tick = hist.clone();
    let stats_for_tick = Arc::clone(stats);
    handle.schedule_recurring(initial_delay_ms, interval_ms, job, move || {
        heavy_cron_tick(
            &lc,
            &handle_for_tick,
            &hist_for_tick,
            &stats_for_tick,
            job,
            cooldown_ms,
            &run_once,
        )
    });
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
                error!(?state, ?event, "Unexpected transition");
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

        // Snapshot this job's outcome generation BEFORE running its closure, so
        // that after a clean return we can tell whether the closure recorded its
        // OWN terminal outcome (an inline gate skip via `record_cron_outcome`,
        // e.g. work-item-presence's DbDown or index-reconcile's PhaseGate). If
        // it did, the default `Ok` below must NOT clobber that `Skipped` in the
        // in-memory `last_cron_outcomes` snapshot (the durable `cron_run_history`
        // ledger is written separately via `record_skip` and stays correct
        // regardless). Heavy crons are unaffected: their gate runs in a pool task
        // that records AFTER this returns, so the default `Ok` is written then
        // overwritten — exactly as before. See the `try_heavy_gate` comment.
        let outcome_seq_before = stats
            .as_ref()
            .map(|s| s.cron_outcome_seq_for(&task_name))
            .unwrap_or(0);

        let started = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (task.task)()));
        let elapsed_ms = started.elapsed().as_millis() as u64;

        if let Some(s) = stats {
            s.cron_executions.fetch_add(1, AtomicOrdering::Relaxed);
        }

        match result {
            Ok(should_requeue) => {
                if let Some(s) = stats
                    && s.cron_outcome_seq_for(&task_name) == outcome_seq_before
                {
                    // The closure recorded no outcome of its own this tick → the
                    // body ran to completion, so default to Ok.
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
    clients_config: &crate::config::ClientsConfig,
    rt: tokio::runtime::Handle,
    embed_tx: crossbeam_channel::Sender<EmbedIndexRequest>,
    lifecycle: DaemonLifecycle,
    cron_pool: Arc<crate::work_pool::pool::WorkPool>,
    general_pool: Option<Arc<crate::work_pool::pool::WorkPool>>,
    system_ctx: crate::context::SystemContext,
    last_runs: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
) {
    // Restart-survival initial-delay helper (ADR-018 §5): every recurring cron's
    // first-tick delay is computed from its last persisted successful completion
    // (`last_runs`, read at startup in `daemon.rs`) rather than a fresh stagger,
    // so a daemon restart no longer slips the schedule. Collapses to ~stagger for
    // first-boot / overdue / short-interval jobs. Keeps call sites one-liners.
    let initial_delay = |job: &str, interval_ms: u64| -> u64 {
        restart_initial_delay_ms(
            job,
            interval_ms,
            last_runs.get(job).map(|t| t.timestamp_millis() as u64),
            now_ms(),
        )
    };

    // Stats aggregation (light — runs unconditionally). Fixed 1s startup delay
    // kept (restart-survival is irrelevant at this cadence); runs recorded.
    let stats_clone = Arc::clone(&stats);
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    let hist_stats_agg = system_ctx.cron_history().clone();
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
            let hist = hist_stats_agg.clone();
            crate::cron::history::spawn_recorded(
                &rt_clone,
                hist,
                "stats-aggregation",
                async move {
                    if let Ok(count) = db.count_indexed_files().await {
                        stats
                            .files_indexed
                            .store(count, std::sync::atomic::Ordering::Relaxed);
                    }
                },
            );
            true
        },
    );

    // Stale file cleanup + orphaned project cleanup (light — runs unconditionally)
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    let hist_stale = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        5000,
        config.stale_cleanup_interval_secs * 1000,
        "stale-cleanup",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            let hist = hist_stale.clone();
            crate::cron::history::spawn_recorded(&rt_clone, hist, "stale-cleanup", async move {
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
    let hist_wip = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        initial_delay("work-item-presence", presence_interval * 1000),
        presence_interval * 1000,
        "work-item-presence",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let stats = Arc::clone(&stats_clone);
            let hist = hist_wip.clone();
            if !stats.db_health().is_up() {
                stats.record_cron_outcome(
                    "work-item-presence",
                    crate::stats::tracker::CronJobOutcome::Skipped(
                        crate::stats::tracker::SkipReason::DbDown,
                    ),
                    0,
                );
                hist.record_skip(
                    "work-item-presence",
                    crate::cron::history::CronTriggerSource::Scheduled,
                    crate::stats::tracker::SkipReason::DbDown,
                );
                return true;
            }
            if let Some(pool) = db_clone.pool().cloned() {
                crate::cron::history::spawn_recorded(
                    &rt_clone,
                    hist,
                    "work-item-presence",
                    async move {
                        crate::cron::work_item_presence::run_or_log(
                            pool,
                            stats,
                            presence_idle,
                            presence_offline,
                        )
                        .await;
                    },
                );
            }
            true
        },
    );

    // mcp-client-liveness: re-check each connected client's PID via /proc,
    // refresh cwd/project for survivors, and flip dead clients to `exited`.
    // Light job; backs the `active_clients` tool + the A2A
    // active-agents-by-project view.
    let db_lv = Arc::clone(&db);
    let rt_lv = rt.clone();
    let stats_lv = Arc::clone(&stats);
    let lc_lv = lifecycle.clone();
    let liveness_interval = config.mcp_client_liveness_interval_secs.max(1);
    let liveness_proc_fd = clients_config.proc_fd_supplement;
    let hist_liveness = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        initial_delay("mcp-client-liveness", liveness_interval * 1000),
        liveness_interval * 1000,
        "mcp-client-liveness",
        move || {
            if lc_lv.is_stopping() {
                return false;
            }
            let stats = Arc::clone(&stats_lv);
            let hist = hist_liveness.clone();
            if !stats.db_health().is_up() {
                stats.record_cron_outcome(
                    "mcp-client-liveness",
                    crate::stats::tracker::CronJobOutcome::Skipped(
                        crate::stats::tracker::SkipReason::DbDown,
                    ),
                    0,
                );
                hist.record_skip(
                    "mcp-client-liveness",
                    crate::cron::history::CronTriggerSource::Scheduled,
                    crate::stats::tracker::SkipReason::DbDown,
                );
                return true;
            }
            if let Some(pool) = db_lv.pool().cloned() {
                crate::cron::history::spawn_recorded(
                    &rt_lv,
                    hist,
                    "mcp-client-liveness",
                    async move {
                        crate::cron::mcp_client_liveness::run_or_log(pool, stats, liveness_proc_fd)
                            .await;
                    },
                );
            }
            true
        },
    );

    // project-deps-index: re-parse Cargo manifests into project_dependencies
    // (source=cargo), feeding the project_depends_on unified-graph edges.
    let db_pd = Arc::clone(&db);
    let rt_pd = rt.clone();
    let stats_pd = Arc::clone(&stats);
    let lc_pd = lifecycle.clone();
    let deps_interval = config.project_deps_index_interval_secs.max(60);
    let hist_pd = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        initial_delay("project-deps-index", deps_interval * 1000),
        deps_interval * 1000,
        "project-deps-index",
        move || {
            if lc_pd.is_stopping() {
                return false;
            }
            let stats = Arc::clone(&stats_pd);
            let hist = hist_pd.clone();
            if let Some(pool) = db_pd.pool().cloned() {
                crate::cron::history::spawn_recorded(
                    &rt_pd,
                    hist,
                    "project-deps-index",
                    async move {
                        crate::cron::project_deps_index::run_or_log(pool, stats).await;
                    },
                );
            }
            true
        },
    );

    // git-state-scan: detect when a dependency under active coordination is back
    // on its stable branch & clean, then resolve the coordination (the gatekeeper
    // close-the-loop). Scoped to active coordinations, so cheap + responsive.
    let db_gs = Arc::clone(&db);
    let rt_gs = rt.clone();
    let stats_gs = Arc::clone(&stats);
    let lc_gs = lifecycle.clone();
    let gitscan_interval = config.git_state_scan_interval_secs.max(15);
    let hist_gs = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        initial_delay("git-state-scan", gitscan_interval * 1000),
        gitscan_interval * 1000,
        "git-state-scan",
        move || {
            if lc_gs.is_stopping() {
                return false;
            }
            let stats = Arc::clone(&stats_gs);
            let hist = hist_gs.clone();
            if !stats.db_health().is_up() {
                stats.record_cron_outcome(
                    "git-state-scan",
                    crate::stats::tracker::CronJobOutcome::Skipped(
                        crate::stats::tracker::SkipReason::DbDown,
                    ),
                    0,
                );
                hist.record_skip(
                    "git-state-scan",
                    crate::cron::history::CronTriggerSource::Scheduled,
                    crate::stats::tracker::SkipReason::DbDown,
                );
                return true;
            }
            if let Some(pool) = db_gs.pool().cloned() {
                crate::cron::history::spawn_recorded(&rt_gs, hist, "git-state-scan", async move {
                    crate::cron::git_state_scan::run_or_log(pool, stats).await;
                });
            }
            true
        },
    );

    // retrieval-eval: periodically score the frozen retrieval-drift probe set
    // through `semantic_search` and record
    // `pgmcp_metadata['retrieval_eval_last_report']`, warning below the floor —
    // the runtime complement to the CI regression gate. Interval-gated (0
    // disables, the default). Pulls the query embedder from the SystemContext.
    if config.retrieval_eval_interval_secs > 0 {
        let db_re = Arc::clone(&db);
        let rt_re = rt.clone();
        let stats_re = Arc::clone(&stats);
        let lc_re = lifecycle.clone();
        let re_interval = config.retrieval_eval_interval_secs;
        let re_embed = system_ctx.embed().clone();
        let re_project = config.retrieval_eval_project.clone();
        let hist_re = system_ctx.cron_history().clone();
        handle.schedule_recurring(
            initial_delay("retrieval-eval", re_interval * 1000),
            re_interval * 1000,
            "retrieval-eval",
            move || {
                if lc_re.is_stopping() {
                    return false;
                }
                let stats = Arc::clone(&stats_re);
                let hist = hist_re.clone();
                if !stats.db_health().is_up() {
                    stats.record_cron_outcome(
                        "retrieval-eval",
                        crate::stats::tracker::CronJobOutcome::Skipped(
                            crate::stats::tracker::SkipReason::DbDown,
                        ),
                        0,
                    );
                    hist.record_skip(
                        "retrieval-eval",
                        crate::cron::history::CronTriggerSource::Scheduled,
                        crate::stats::tracker::SkipReason::DbDown,
                    );
                    return true;
                }
                let embed = re_embed.clone();
                let project = re_project.clone();
                if let Some(pool) = db_re.pool().cloned() {
                    crate::cron::history::spawn_recorded(
                        &rt_re,
                        hist,
                        "retrieval-eval",
                        async move {
                            crate::cron::retrieval_eval::run_or_log(pool, embed, stats, &project)
                                .await;
                        },
                    );
                }
                true
            },
        );
    }

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
        let hist_fp = system_ctx.cron_history().clone();
        handle.schedule_recurring(
            initial_delay("findings-promotion", fp_interval * 1000),
            fp_interval * 1000,
            "findings-promotion",
            move || {
                if lc_fp.is_stopping() {
                    return false;
                }
                let stats = Arc::clone(&stats_clone_fp);
                let hist = hist_fp.clone();
                if let Some(pool) = db_clone_fp.pool().cloned() {
                    crate::cron::history::spawn_recorded(
                        &rt_clone_fp,
                        hist,
                        "findings-promotion",
                        async move {
                            crate::cron::findings_promotion::run_or_log(pool, stats).await;
                        },
                    );
                }
                true
            },
        );
    }

    // concurrency-scan (ADR-011): lock-order + channel deadlock analysis →
    // `concurrency_findings` ledger + bitemporal `lock_order_edges` + health
    // snapshots (Layer 4). Opt-in, default OFF (interval 0).
    if config.concurrency_scan_interval_secs > 0 {
        let db_clone_cs = Arc::clone(&db);
        let rt_clone_cs = rt.clone();
        let stats_clone_cs = Arc::clone(&stats);
        let lc_cs = lifecycle.clone();
        let cs_interval = config.concurrency_scan_interval_secs;
        let cs_promote = config.concurrency_auto_promote;
        let hist_cs = system_ctx.cron_history().clone();
        handle.schedule_recurring(
            initial_delay("concurrency-scan", cs_interval * 1000),
            cs_interval * 1000,
            "concurrency-scan",
            move || {
                if lc_cs.is_stopping() {
                    return false;
                }
                let stats = Arc::clone(&stats_clone_cs);
                let hist = hist_cs.clone();
                if let Some(pool) = db_clone_cs.pool().cloned() {
                    crate::cron::history::spawn_recorded(
                        &rt_clone_cs,
                        hist,
                        "concurrency-scan",
                        async move {
                            crate::cron::concurrency_scan::run_or_log(pool, stats, cs_promote)
                                .await;
                        },
                    );
                }
                true
            },
        );
    }

    // target-cleanup (disk reclamation): tiered removal of regeneratable Rust
    // `target/` build artifacts + a provenance-first `/tmp`+`/var/tmp` sweep.
    // Light I/O job — runs on the runtime like the presence/findings sweeps (no
    // heavy-cron gate); the blocking filesystem work is dispatched via
    // spawn_blocking inside run_or_log. Ships enabled-but-dry-run; interval 0
    // disables. A fixed ~10-min initial delay surfaces the first dry-run
    // manifest soon after a restart (the projects table persists across
    // restarts, so discovery is populated immediately) without colliding with
    // the startup scan storm; the configured cadence (default weekly) thereafter.
    if config.target_cleanup.interval_secs > 0 {
        let db_clone_tc = Arc::clone(&db);
        let rt_clone_tc = rt.clone();
        let lc_tc = lifecycle.clone();
        let tc_interval = config.target_cleanup.interval_secs;
        let tc_cfg = config.target_cleanup.clone();
        let hist_tc = system_ctx.cron_history().clone();
        handle.schedule_recurring(
            // Fixed 10-minute delay kept intentionally: target-cleanup must
            // surface its first dry-run manifest soon after every restart, so it
            // deliberately does NOT use restart-survival (which would defer it to
            // the next weekly due-time).
            600_000,
            tc_interval * 1000,
            "target-cleanup",
            move || {
                if lc_tc.is_stopping() {
                    return false;
                }
                let cfg = tc_cfg.clone();
                let hist = hist_tc.clone();
                if let Some(pool) = db_clone_tc.pool().cloned() {
                    crate::cron::history::spawn_recorded_with(
                        &rt_clone_tc,
                        hist,
                        "target-cleanup",
                        async move {
                            crate::cron::target_cleanup::run_or_log(pool, cfg)
                                .await
                                .to_counters()
                        },
                    );
                }
                true
            },
        );
    }

    // docker-cleanup: bounded reclamation of Docker's build cache + dangling
    // images (never tagged images, running containers, or named volumes). Light
    // job — shells to `docker`, no DB, no Ready gate; ships enabled-but-dry-run.
    // interval 0 disables. Uses the standard restart-survival initial delay.
    if config.docker_cleanup.interval_secs > 0 {
        let rt_clone_dc = rt.clone();
        let lc_dc = lifecycle.clone();
        let dc_interval = config.docker_cleanup.interval_secs;
        let dc_cfg = config.docker_cleanup.clone();
        let hist_dc = system_ctx.cron_history().clone();
        handle.schedule_recurring(
            initial_delay("docker-cleanup", dc_interval * 1000),
            dc_interval * 1000,
            "docker-cleanup",
            move || {
                if lc_dc.is_stopping() {
                    return false;
                }
                let cfg = dc_cfg.clone();
                let hist = hist_dc.clone();
                crate::cron::history::spawn_recorded_with(
                    &rt_clone_dc,
                    hist,
                    "docker-cleanup",
                    async move {
                        crate::cron::docker_cleanup::run_or_log(cfg)
                            .await
                            .to_counters()
                    },
                );
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
    let hist_integrity = system_ctx.cron_history().clone();
    handle.schedule_recurring(
        config.integrity_check_interval_secs * 1000,
        config.integrity_check_interval_secs * 1000,
        "integrity-check",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            let hist = hist_integrity.clone();
            crate::cron::history::spawn_recorded(&rt_clone, hist, "integrity-check", async move {
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

    // DB maintenance (VACUUM ANALYZE) (light — runs unconditionally). Also sweeps
    // `cron_run_history` past its retention window (ADR-018 §9).
    let db_clone = Arc::clone(&db);
    let rt_clone = rt.clone();
    let lc = lifecycle.clone();
    let hist_dbm = system_ctx.cron_history().clone();
    let cron_history_retention_days = config.cron_history_retention_days;
    handle.schedule_recurring(
        config.db_maintenance_interval_secs * 1000,
        config.db_maintenance_interval_secs * 1000,
        "db-maintenance",
        move || {
            if lc.is_stopping() {
                return false;
            }
            let db = db_clone.clone();
            let hist = hist_dbm.clone();
            crate::cron::history::spawn_recorded(&rt_clone, hist, "db-maintenance", async move {
                let pool = db.pool().expect("inline SQL needs PgPool");
                if let Err(e) = sqlx::query("VACUUM ANALYZE indexed_files")
                    .execute(pool)
                    .await
                {
                    tracing::error!("DB maintenance failed: {}", e);
                }
                if let Err(e) = sqlx::query("VACUUM ANALYZE file_chunks")
                    .execute(pool)
                    .await
                {
                    tracing::error!("DB maintenance (chunks) failed: {}", e);
                }
                // Retention sweep for the cron-run-history ledger (0 = keep forever).
                match crate::db::queries::delete_cron_runs_older_than(
                    pool,
                    cron_history_retention_days,
                )
                .await
                {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, "Swept aged cron_run_history rows")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!("cron_run_history retention sweep failed: {}", e),
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
    let heavy_cron_lock = Arc::clone(system_ctx.heavy_cron_lock());
    // Dedicated lock for the cheap, read-mostly quality-history snapshot. It was
    // previously on `heavy_cron_lock`, where it lost the try_lock race to the
    // 20-30 min GPU jobs (topic-clustering / graph-analysis) every tick and
    // starved (observed: `quality_history_runs=1`, then perpetual
    // `skipped:lock_busy`), leaving quality_trend / quality_forecast / burndown
    // and the digest with an empty series. It only reads tables (MVCC-safe
    // alongside the GPU writers) and appends to quality_report_history, so it
    // needs to exclude only itself — its own lock decouples it from the GPU herd.
    let quality_history_lock: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

    // Git history indexing (heavy). Restart-survival initial delay + §6
    // honor-settle cooldown + durable run recording are all provided by
    // `register_heavy_cron`; the closure below is just the per-cron work, which
    // records its outcome on the `CronRunGuard` (`run`). RSS / duration / thread
    // deltas are captured by the guard, replacing the old manual rss logging.
    let git_interval_ms = config.git_history_index_interval_secs * 1000;
    let git_cooldown_ms = config.ready_delay_git_secs * 1000;
    let stats_for_git = Arc::clone(&stats);
    let db_for_git = db_clone.clone();
    let commit_tx_for_git = commit_tx.clone();
    let rt_for_git = rt_clone.clone();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "git-history-index",
        initial_delay("git-history-index", git_interval_ms),
        git_interval_ms,
        git_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_git);
            let db = db_for_git.clone();
            let tx = commit_tx_for_git.clone();
            let rt = rt_for_git.clone();

            // Top-of-body counter: a reliable "this cron's body ran" signal.
            // Pairs with `git_history_noop_returns` for the empty-data case.
            stats.git_history_runs.fetch_add(1, AtomicOrdering::Relaxed);

            // Once-per-tick `git` binary preflight. A missing `git` is a
            // permanent fault — `classify_io_error` maps `NotFound` to `Disable`;
            // record the reason so the scheduler skip-check elides future runs.
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
                stats.disable_cron_job("git-history-index", reason.clone());
                run.fail(reason);
                return;
            }

            // Ok(true)=noop (no git projects) | Ok(false)=ran | Err=list failed.
            let mut body_result: Result<bool, String> = Ok(false);
            rt.block_on(async {
                match db.get_git_enabled_projects().await {
                    Ok(projects) if projects.is_empty() => {
                        stats
                            .git_history_noop_returns
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        tracing::info!("git-history-index: no git-enabled projects, nothing to do");
                        body_result = Ok(true);
                    }
                    Ok(projects) => {
                        for (project_id, project_path) in &projects {
                            let project_root = std::path::Path::new(project_path);
                            if !crate::indexer::git_indexer::is_git_history_enabled(project_root) {
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
                        body_result = Err(format!("list git-enabled projects: {e}"));
                    }
                }
            });
            match body_result {
                Ok(true) => run.noop(),
                Ok(false) => run.ok(),
                Err(msg) => run.fail(msg),
            }
        },
    );

    // Cross-project similarity scan (heavy). Intrinsics + §6 honor-settle +
    // restart-survival via register_heavy_cron; the closure is the per-cron work.
    let sim_interval_ms = config.similarity_scan_interval_secs * 1000;
    let sim_cooldown_ms = config.ready_delay_similarity_secs * 1000;
    let db_clone_sim = Arc::clone(&db);
    let rt_clone_sim = rt.clone();
    let stats_for_sim = Arc::clone(&stats);
    let lc_for_sim = lifecycle.clone();
    let sim_cron_config = CronConfig {
        similarity_scan_interval_secs: config.similarity_scan_interval_secs,
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
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "similarity-scan",
        initial_delay("similarity-scan", sim_interval_ms),
        sim_interval_ms,
        sim_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_sim);
            let db = db_clone_sim.clone();
            let cfg = sim_cron_config.clone();
            let rt = rt_clone_sim.clone();
            let lc_inner = lc_for_sim.clone();
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
            run.ok();
        },
    );

    // Semantic-edges materialization (heavy — gates on Ready + heavy_cron_lock).
    // Sequenced (via its ready-delay) before graph-analysis so its edges are
    // present for the blended PageRank / betweenness / community pass.
    let sem_interval_ms = config.semantic_edge_interval_secs * 1000;
    let sem_cooldown_ms = config.ready_delay_semantic_secs * 1000;
    let db_clone_sem = Arc::clone(&db);
    let rt_clone_sem = rt.clone();
    let stats_for_sem = Arc::clone(&stats);
    let lc_for_sem = lifecycle.clone();
    let sem_cron_config = config.clone();
    let sem_ef_search = 100; // default ef_search (mirrors similarity-scan)
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "semantic-edges",
        initial_delay("semantic-edges", sem_interval_ms),
        sem_interval_ms,
        sem_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_sem);
            let db = db_clone_sem.clone();
            let cfg = sem_cron_config.clone();
            let rt = rt_clone_sem.clone();
            let lc_inner = lc_for_sem.clone();
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
            run.ok();
        },
    );

    // Graph analysis (import extraction, PageRank, betweenness, coupling)
    let graph_interval_ms = config.graph_analysis_interval_secs * 1000;
    let graph_cooldown_ms = config.ready_delay_graph_secs * 1000;
    let db_clone_graph = Arc::clone(&db);
    let rt_clone_graph = rt.clone();
    let stats_for_graph = Arc::clone(&stats);
    let graph_general_pool = general_pool.clone();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "graph-analysis",
        initial_delay("graph-analysis", graph_interval_ms),
        graph_interval_ms,
        graph_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_graph);
            let db = db_clone_graph.clone();
            let wp = graph_general_pool.clone();
            let rt = rt_clone_graph.clone();
            rt.block_on(async {
                crate::cron::graph_analysis::run_graph_analysis(db.as_ref(), &stats, wp).await;
            });
            run.ok();
        },
    );

    // Symbol extraction (Tier-0e tree-sitter pass — populates file_symbols + symbol_references)
    let symbol_extraction_interval_ms = config.symbol_extraction_interval_secs * 1000;
    let symbol_extraction_cooldown_ms = config.ready_delay_symbol_extraction_secs * 1000;
    let db_clone_symbol = Arc::clone(&db);
    let rt_clone_symbol = rt.clone();
    let stats_for_symbol = Arc::clone(&stats);
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "symbol-extraction",
        initial_delay("symbol-extraction", symbol_extraction_interval_ms),
        symbol_extraction_interval_ms,
        symbol_extraction_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_symbol);
            let db = db_clone_symbol.clone();
            let rt = rt_clone_symbol.clone();
            rt.block_on(async {
                crate::cron::symbol_extraction::run_symbol_extraction(db.as_ref(), &stats).await;
            });
            run.ok();
        },
    );

    // SOTA Phase 1 — Function metrics cron (CC / Cognitive / Halstead / NPath / MI per function).
    // Sequenced after symbol-extraction (depends on file_symbols rows).
    let function_metrics_interval_ms = config.function_metrics_interval_secs * 1000;
    let fnmet_cooldown_ms = config.ready_delay_function_metrics_secs * 1000;
    let db_clone_fnmet = Arc::clone(&db);
    let rt_clone_fnmet = rt.clone();
    let stats_for_fnmet = Arc::clone(&stats);
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "function-metrics",
        initial_delay("function-metrics", function_metrics_interval_ms),
        function_metrics_interval_ms,
        fnmet_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_fnmet);
            let db = db_clone_fnmet.clone();
            let rt = rt_clone_fnmet.clone();
            rt.block_on(async {
                crate::cron::function_metrics::run_function_metrics(db.as_ref(), &stats).await;
            });
            run.ok();
        },
    );

    // SOTA Phase 1 — Call-graph cron (symbol-resolved edges + fan_in/fan_out).
    // Sequenced after function-metrics (which depends on symbol-extraction's
    // file_symbols rows and seeds function_metrics rows for the fan_in/fan_out
    // UPDATE this cron issues).
    let db_clone_cg = Arc::clone(&db);
    let call_graph_interval_ms = config.call_graph_interval_secs * 1000;
    let cg_cooldown_ms = config.ready_delay_call_graph_secs * 1000;
    let rt_clone_cg = rt.clone();
    let stats_for_cg = Arc::clone(&stats);
    // Same WorkPool the file-graph cron uses for parallel Brandes betweenness;
    // the call-graph cron now runs betweenness over the function call graph too.
    let cg_general_pool = general_pool.clone();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "call-graph",
        initial_delay("call-graph", call_graph_interval_ms),
        call_graph_interval_ms,
        cg_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_cg);
            let db = db_clone_cg.clone();
            let rt = rt_clone_cg.clone();
            let wp = cg_general_pool.clone();
            rt.block_on(async {
                crate::cron::call_graph::run_call_graph(db.as_ref(), &stats, wp).await;
            });
            run.ok();
        },
    );

    // RAPTOR-over-code summary tree (heavy — CUDA FCM per project; gates on
    // Ready + heavy_cron_lock). Sequenced (via ready-delay) after topic-
    // clustering so embeddings are settled before the conceptual tree is built.
    let raptor_interval_ms = config.code_raptor_interval_secs * 1000;
    let raptor_cooldown_ms = config.ready_delay_code_raptor_secs * 1000;
    let db_clone_raptor = Arc::clone(&db);
    let rt_clone_raptor = rt.clone();
    let stats_for_raptor = Arc::clone(&stats);
    let lc_for_raptor = lifecycle.clone();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "code-raptor",
        initial_delay("code-raptor", raptor_interval_ms),
        raptor_interval_ms,
        raptor_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_raptor);
            let db = db_clone_raptor.clone();
            let rt = rt_clone_raptor.clone();
            let lc_inner = lc_for_raptor.clone();
            rt.block_on(async {
                crate::cron::code_raptor::run_code_raptor(db.as_ref(), &stats, &lc_inner).await;
            });
            run.ok();
        },
    );

    // Fuzzy-index sync (heavy). The `CronRunGuard` now captures the rss/thread
    // deltas that were the persistent-trie daemon-thread leak signal (a healthy
    // run returns threads_delta to ~0; a steadily-climbing delta means handles
    // aren't being reclaimed); the per-run report counts become `counters`.
    let fuzzy_interval_ms = config.fuzzy_sync_interval_secs * 1000;
    let fuzzy_cooldown_ms = config.ready_delay_topic_secs * 1000;
    let db_clone_fuzzy = Arc::clone(&db);
    let rt_clone_fuzzy = rt.clone();
    let stats_for_fuzzy = Arc::clone(&stats);
    let fuzzy_data_dir = fuzzy_config.data_dir.clone();
    let fuzzy_max_disk_bytes = fuzzy_config.max_disk_bytes;
    let fuzzy_eviction_cfg = fuzzy_config.eviction_config();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "fuzzy-sync",
        initial_delay("fuzzy-sync", fuzzy_interval_ms),
        fuzzy_interval_ms,
        fuzzy_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_fuzzy);
            let db = db_clone_fuzzy.clone();
            let rt = rt_clone_fuzzy.clone();
            let data_dir = fuzzy_data_dir.clone();
            let max_disk_bytes = fuzzy_max_disk_bytes;
            let eviction_cfg = fuzzy_eviction_cfg.clone();
            let Some(pool) = db.pool().cloned() else {
                tracing::warn!(job = "fuzzy-sync", "skipping run: DbClient has no pool");
                run.noop();
                return;
            };
            let result = rt.block_on(async move {
                crate::cron::fuzzy_sync::run_fuzzy_sync(
                    &pool,
                    &data_dir,
                    max_disk_bytes,
                    eviction_cfg,
                    stats,
                )
                .await
            });
            match result {
                Ok(report) => {
                    tracing::info!(
                        job = "fuzzy-sync",
                        symbols = report.symbols_synced,
                        paths = report.paths_synced,
                        commits = report.commits_synced,
                        durable_mandates = report.durable_mandates_synced,
                        concepts = report.concepts_synced,
                        "fuzzy-sync run complete"
                    );
                    run.ok_with(serde_json::json!({
                        "symbols": report.symbols_synced,
                        "paths": report.paths_synced,
                        "commits": report.commits_synced,
                        "durable_mandates": report.durable_mandates_synced,
                        "concepts": report.concepts_synced,
                    }));
                }
                Err(e) => {
                    tracing::error!(job = "fuzzy-sync", error = %e, "fuzzy-sync run failed");
                    run.fail(e.to_string());
                }
            }
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
        // Own lock (`quality_history_lock`, not heavy_cron_lock) so the cheap
        // snapshot stops starving behind multi-minute GPU crons; 120 s settle.
        let qh_interval_ms = config.quality_history_interval_secs * 1000;
        let rt_clone_qh = rt.clone();
        let stats_for_qh = Arc::clone(&stats);
        let ctx_qh = system_ctx.clone();
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &quality_history_lock,
            &stats,
            system_ctx.cron_history(),
            "quality-history",
            initial_delay("quality-history", qh_interval_ms),
            qh_interval_ms,
            120_000,
            move |run| {
                let stats_run = Arc::clone(&stats_for_qh);
                let rt = rt_clone_qh.clone();
                let ctx = ctx_qh.clone();
                rt.block_on(async move {
                    crate::cron::quality_history::run_or_log(ctx, stats_run).await;
                });
                run.ok();
            },
        );
    }

    // topics-size-history (cheap — snapshots every `code_topics` row's
    // `chunk_count` into `pgmcp_metadata['topics_size_history']` so `topic_trends`
    // reads a per-topic trajectory, not a single point). Own light lock, 120 s
    // settle — same idiom as tool-policy-refresh.
    if config.topics_size_history_interval_secs > 0 {
        let tsh_interval_ms = config.topics_size_history_interval_secs * 1000;
        let lock_tsh: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));
        let rt_clone_tsh = rt.clone();
        let ctx_tsh = system_ctx.clone();
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &lock_tsh,
            &stats,
            system_ctx.cron_history(),
            "topics-size-history",
            initial_delay("topics-size-history", tsh_interval_ms),
            tsh_interval_ms,
            120_000,
            move |run| {
                let rt = rt_clone_tsh.clone();
                let ctx = ctx_tsh.clone();
                rt.block_on(async move {
                    if let Some(pool) = ctx.db().pool() {
                        crate::cron::topics_size_history::run_or_log(pool).await;
                    }
                });
                run.ok();
            },
        );
    }

    if config.tool_policy_interval_secs > 0 {
        // Cheap learner pass (a few SQL statements) — its own lock so it never
        // starves behind the GPU herd; it only reads `mcp_tool_calls` and rewrites
        // the small `client_tool_policy` table. 120 s settle.
        let tp_interval_ms = config.tool_policy_interval_secs * 1000;
        let lock_tp: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));
        let rt_clone_tp = rt.clone();
        let stats_for_tp = Arc::clone(&stats);
        let ctx_tp = system_ctx.clone();
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &lock_tp,
            &stats,
            system_ctx.cron_history(),
            "tool-policy-refresh",
            initial_delay("tool-policy-refresh", tp_interval_ms),
            tp_interval_ms,
            120_000,
            move |run| {
                let stats_run = Arc::clone(&stats_for_tp);
                let rt = rt_clone_tp.clone();
                let ctx = ctx_tp.clone();
                rt.block_on(async move {
                    crate::cron::tool_policy_refresh::run_or_log(ctx, stats_run).await;
                });
                run.ok();
            },
        );
    }

    if config.embedding_migration_interval_secs > 0 {
        // F6 (boy-scout 2026-05-25): use the migration-specific ready-delay
        // (default 60s) instead of reusing `ready_delay_topic_secs` (default
        // 3600s). Migration has nothing to wait for post-Ready — it just drains
        // rows whose `embedding_v2` column is NULL.
        let mig_interval_ms = config.embedding_migration_interval_secs * 1000;
        let mig_cooldown_ms = config.ready_delay_embedding_migration_secs * 1000;
        let db_clone_mig = Arc::clone(&db);
        let rt_clone_mig = rt.clone();
        let stats_for_mig = Arc::clone(&stats);
        let mig_cfg = crate::cron::embedding_migration::EmbeddingMigrationConfig::new(
            embeddings_config.clone(),
            config.embedding_migration_batch_size,
            config.embedding_migration_max_batches,
        );
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &heavy_cron_lock,
            &stats,
            system_ctx.cron_history(),
            "embedding-migration",
            initial_delay("embedding-migration", mig_interval_ms),
            mig_interval_ms,
            mig_cooldown_ms,
            move |run| {
                let stats = Arc::clone(&stats_for_mig);
                let db = db_clone_mig.clone();
                let rt = rt_clone_mig.clone();
                let mig_cfg = mig_cfg.clone();
                let Some(pool) = db.pool().cloned() else {
                    tracing::warn!(
                        job = "embedding-migration",
                        "skipping run: DbClient has no pool"
                    );
                    run.noop();
                    return;
                };
                rt.block_on(async move {
                    crate::cron::embedding_migration::run_or_log(Arc::new(pool), stats, mig_cfg)
                        .await;
                });
                run.ok();
            },
        );
    }

    // ngram-lm-train cron — per-project HybridLM training (n-gram +
    // subword embedding) used by the third RRF leg of
    // `tool_hybrid_search` and by `tool_correct_query`. Off when
    // interval = 0.
    if config.ngram_lm_train_interval_secs > 0 {
        let lm_interval_ms = config.ngram_lm_train_interval_secs * 1000;
        let lm_cooldown_ms = config.ready_delay_topic_secs * 1000;
        let db_clone_lm = Arc::clone(&db);
        let rt_clone_lm = rt.clone();
        let stats_for_lm = Arc::clone(&stats);
        let lm_data_dir = fuzzy_config.data_dir.clone();
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &heavy_cron_lock,
            &stats,
            system_ctx.cron_history(),
            "ngram-lm-train",
            initial_delay("ngram-lm-train", lm_interval_ms),
            lm_interval_ms,
            lm_cooldown_ms,
            move |run| {
                let stats = Arc::clone(&stats_for_lm);
                let db = db_clone_lm.clone();
                let rt = rt_clone_lm.clone();
                let data_dir = lm_data_dir.clone();
                let Some(pool) = db.pool().cloned() else {
                    tracing::warn!(job = "ngram-lm-train", "skipping run: DbClient has no pool");
                    run.noop();
                    return;
                };
                rt.block_on(async move {
                    crate::cron::ngram_lm_train::run_or_log(Arc::new(pool), stats, data_dir).await;
                });
                run.ok();
            },
        );
    }

    // Topic-dendrogram cron — hierarchical-agglomerative + c-TF-IDF
    // built on top of the same chunks the online FCM owns. Persists
    // to `topic_dendrograms`; the `dendrogram_topic_hierarchy` MCP
    // tool reads from there. Off when interval = 0.
    if config.topic_dendrogram_interval_secs > 0 {
        let td_interval_ms = config.topic_dendrogram_interval_secs * 1000;
        let td_cooldown_ms = config.ready_delay_topic_secs * 1000;
        let db_clone_td = Arc::clone(&db);
        let rt_clone_td = rt.clone();
        let stats_for_td = Arc::clone(&stats);
        register_heavy_cron(
            handle,
            &lifecycle,
            &cron_pool,
            &heavy_cron_lock,
            &stats,
            system_ctx.cron_history(),
            "topic-dendrogram",
            initial_delay("topic-dendrogram", td_interval_ms),
            td_interval_ms,
            td_cooldown_ms,
            move |run| {
                let stats = Arc::clone(&stats_for_td);
                let db = db_clone_td.clone();
                let rt = rt_clone_td.clone();
                let Some(pool) = db.pool().cloned() else {
                    tracing::warn!(
                        job = "topic-dendrogram",
                        "skipping run: DbClient has no pool"
                    );
                    run.noop();
                    return;
                };
                rt.block_on(async move {
                    crate::cron::topic_dendrogram::run_or_log(Arc::new(pool), stats).await;
                });
                run.ok();
            },
        );
    }

    // Topic clustering (global full-chunk — always produces scope = "global").
    // This is the motivating cron for ADR-018: an overdue topic-clustering after
    // restart used to slip ~12h because its stagger tick was cooldown-gated; the
    // §6 honor-settle retry now runs it ~1h after Ready instead.
    let topic_interval_ms = config.topic_scan_interval_secs * 1000;
    let topic_cooldown_ms = config.ready_delay_topic_secs * 1000;
    let db_clone_topic = Arc::clone(&db);
    let rt_clone_topic = rt.clone();
    let stats_for_topic = Arc::clone(&stats);
    let lc_for_topic = lifecycle.clone();
    // Thread the FULL topic config (engine `topic_clustering_method`, gate
    // thresholds, LLM-label toggle, reducer dims, graph edge weights, …) from the
    // loaded TOML. The prior field-by-field literal with `..CronConfig::default()`
    // silently reset every un-listed knob back to its default — including
    // `topic_clustering_method`, the engine the effective staleness signature is
    // keyed on — so an operator's engine/gate overrides were ignored by the cron
    // and the stamped signature could disagree with what the consumers compute.
    // Mirrors `sem_cron_config = config.clone()` above.
    let topic_cron_config = config.clone();
    register_heavy_cron(
        handle,
        &lifecycle,
        &cron_pool,
        &heavy_cron_lock,
        &stats,
        system_ctx.cron_history(),
        "topic-clustering",
        initial_delay("topic-clustering", topic_interval_ms),
        topic_interval_ms,
        topic_cooldown_ms,
        move |run| {
            let stats = Arc::clone(&stats_for_topic);
            let db = db_clone_topic.clone();
            let cfg = topic_cron_config.clone();
            let rt = rt_clone_topic.clone();
            let lc_inner = lc_for_topic.clone();
            rt.block_on(async {
                crate::cron::topic_clustering::run_global_topic_scan(
                    db.as_ref(),
                    &cfg,
                    &stats,
                    &lc_inner,
                )
                .await;
            });
            run.ok();
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

    // ---- ADR-018 §5: restart-survival initial delay ----

    #[test]
    fn restart_delay_no_history_is_stagger() {
        // First boot / unknown job → fall back to the anti-herd stagger.
        let ivl = 43_200_000; // 12h
        assert_eq!(
            restart_initial_delay_ms("topic-clustering", ivl, None, 1_000_000_000),
            staggered_initial_delay_ms("topic-clustering", ivl)
        );
    }

    #[test]
    fn restart_delay_overdue_is_stagger() {
        // last_ok + interval already in the past → fire soon, but staggered.
        let ivl = 43_200_000;
        let now = 1_000_000_000;
        let last_ok = now - ivl - 5_000; // overdue by 5s
        assert_eq!(
            restart_initial_delay_ms("topic-clustering", ivl, Some(last_ok), now),
            staggered_initial_delay_ms("topic-clustering", ivl)
        );
    }

    #[test]
    fn restart_delay_future_due_waits_remaining_floored_at_stagger() {
        // Recent success → wait out the remaining interval (restart-survival),
        // never shorter than the stagger.
        let ivl = 43_200_000;
        let now = 1_000_000_000;
        let last_ok = now - 1_000_000; // ran 1000s ago → ~11.7h remaining
        let got = restart_initial_delay_ms("topic-clustering", ivl, Some(last_ok), now);
        let remaining = (last_ok + ivl) - now;
        let stagger = staggered_initial_delay_ms("topic-clustering", ivl);
        assert_eq!(got, remaining.max(stagger));
        assert!(got >= stagger, "must never fire earlier than the stagger");
    }

    #[test]
    fn restart_delay_clock_backwards_is_stagger() {
        // last_ok in the future (clock skew) → stagger fallback, no underflow.
        let ivl = 3_600_000;
        let now = 1_000_000_000;
        assert_eq!(
            restart_initial_delay_ms("git-history-index", ivl, Some(now + 10_000), now),
            staggered_initial_delay_ms("git-history-index", ivl)
        );
    }

    #[test]
    fn restart_delay_zero_interval_is_zero() {
        // Disabled cron → caller won't schedule; the pure fn returns 0.
        assert_eq!(restart_initial_delay_ms("x", 0, Some(1), 2), 0);
        assert_eq!(restart_initial_delay_ms("x", 0, None, 2), 0);
    }

    #[test]
    fn restart_delay_two_overdue_jobs_differ_anti_herd() {
        // Two overdue jobs must not both collapse to the same instant.
        let ivl = 7_200_000;
        let now = 1_000_000_000;
        let last = now - ivl - 1; // both overdue
        let a = restart_initial_delay_ms("symbol-extraction", ivl, Some(last), now);
        let b = restart_initial_delay_ms("function-metrics", ivl, Some(last), now);
        assert_ne!(a, b, "overdue jobs must stagger, not herd");
    }

    // ---- ADR-018 §6: honor-settle cooldown decision ----

    #[test]
    fn cooldown_zero_is_run_now() {
        let lc = crate::daemon_state::DaemonLifecycle::new();
        lc.transition(crate::daemon_state::DaemonPhase::Scanning);
        lc.transition(crate::daemon_state::DaemonPhase::Ready);
        assert!(matches!(
            cooldown_decision(&lc, 0),
            CooldownDecision::RunNow
        ));
    }

    #[test]
    fn cooldown_not_ready_is_run_now() {
        // Pre-Ready ticks defer to the in-pool PhaseGate, not the settle.
        let lc = crate::daemon_state::DaemonLifecycle::new();
        assert!(matches!(
            cooldown_decision(&lc, 3_600_000),
            CooldownDecision::RunNow
        ));
    }

    #[test]
    fn cooldown_ready_within_window_defers() {
        // Just reached Ready with a 1h settle → defer with a positive delay
        // bounded by cooldown + slack.
        let lc = crate::daemon_state::DaemonLifecycle::new();
        lc.transition(crate::daemon_state::DaemonPhase::Scanning);
        lc.transition(crate::daemon_state::DaemonPhase::Ready);
        let cooldown = 3_600_000u64;
        match cooldown_decision(&lc, cooldown) {
            CooldownDecision::DeferMs(d) => {
                assert!(
                    d > 0 && d <= cooldown + COOLDOWN_SLACK_MS,
                    "defer {d} out of range"
                );
            }
            CooldownDecision::RunNow => panic!("expected a defer just after Ready"),
        }
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
        // Wall-clock timing test: a 50 ms interval over a 275 ms sleep is ≈6
        // firings. Under a loaded `verify.sh` run (release build + 1500+ tests +
        // GPU smoke in parallel) CPU contention can stretch the sleep or let the
        // scheduler catch up, so tolerate jitter on both sides while still
        // confirming the task fires repeatedly and is bounded (no runaway).
        assert!(
            (3..=12).contains(&count),
            "Expected 3-12 executions (timing-jitter tolerant), got {}",
            count
        );
    }

    #[test]
    fn test_one_shot_task() {
        let terminating = Arc::new(AtomicBool::new(false));
        let (handle, thread, ready) = spawn_cron_with_interval(Arc::clone(&terminating), 10, None);
        // Wait for the cron thread to be running before scheduling, so the
        // one-shot isn't dropped on the floor before the loop starts.
        ready.recv().expect("Cron thread failed to start");
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        handle.schedule_once(0, "one-shot", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        // Poll (bounded) until the one-shot has run, rather than a fixed sleep
        // that flakes under heavy parallel-test CPU load.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while counter.load(std::sync::atomic::Ordering::Relaxed) == 0
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        handle.request_shutdown();
        thread.join().expect("Cron thread panicked");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Regression: a cron closure that records its OWN terminal outcome (an
    /// inline gate skip) then returns true must keep that outcome in the
    /// in-memory snapshot — the scheduler must NOT clobber it with the default
    /// `Ok`. A closure that records nothing still defaults to `Ok`.
    #[test]
    fn execute_inline_preserves_a_self_recorded_skip() {
        use crate::stats::tracker::{CronJobOutcome, SkipReason, StatsTracker};
        let terminating = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsTracker::new());
        let (handle, thread, ready) =
            spawn_cron_with_interval(Arc::clone(&terminating), 10, Some(Arc::clone(&stats)));
        ready.recv().expect("Cron thread failed to start");

        // Inline-gated light-cron pattern: record a PhaseGate skip, return true.
        let s_skip = Arc::clone(&stats);
        handle.schedule_recurring(0, 10_000, "skipper", move || {
            s_skip.record_cron_outcome(
                "skipper",
                CronJobOutcome::Skipped(SkipReason::PhaseGate),
                0,
            );
            true
        });
        // Plain body that records nothing → scheduler defaults it to Ok.
        handle.schedule_recurring(0, 10_000, "runner", move || true);

        // Bounded wait until both have fired at least once (seq advances on the
        // skip write / the default-Ok write respectively).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while (stats.cron_outcome_seq_for("skipper") == 0
            || stats.cron_outcome_seq_for("runner") == 0)
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        handle.request_shutdown();
        thread.join().expect("Cron thread panicked");

        let skipper = stats
            .last_cron_outcomes
            .get("skipper")
            .map(|r| r.outcome.as_str().to_string());
        let runner = stats
            .last_cron_outcomes
            .get("runner")
            .map(|r| r.outcome.as_str().to_string());
        assert_eq!(
            skipper.as_deref(),
            Some("skipped:phase_gate"),
            "the scheduler must preserve a closure's self-recorded skip, not clobber it with Ok"
        );
        assert_eq!(
            runner.as_deref(),
            Some("ok"),
            "a closure that records nothing this tick still defaults to Ok"
        );
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

    /// Helper: run `try_heavy_gate` and report whether the body would have run
    /// (`Some` lock returned). The cooldown settle is no longer part of the gate
    /// (it moved to `heavy_cron_tick`), so these tests isolate the in-pool gates:
    /// PhaseGate / Shutdown / DbDown / DiskPressure / LockBusy.
    fn try_gate(lc: &DaemonLifecycle, stats: &Arc<StatsTracker>) -> bool {
        let lock = tokio::sync::Mutex::new(());
        let hist = crate::cron::history::CronHistoryWriter::null(Arc::clone(stats));
        try_heavy_gate("heavy-gate-shutdown-test", lc, &lock, stats, &hist).is_some()
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
