//! Outcome enums for cron jobs — extracted from `tracker.rs` as part of
//! the D.2 god-file split.
//!
//! These three types ride on every entry in the `last_cron_outcomes`
//! map: `SkipReason` (why a closure short-circuited), `CronJobOutcome`
//! (the overall envelope), and `CronJobStatus` (the most-recent outcome
//! + when + how long).

use chrono::{DateTime, Utc};

/// Why a heavy-cron closure returned early without entering its work
/// body. Recorded alongside the closure-level `CronJobOutcome::Skipped`
/// so an operator tailing `index_stats` can tell *which* of the three
/// silent-skip paths is currently silencing the job.
///
/// - `PhaseGate`: the `is_at_least(DaemonPhase::Ready)` check rejected
///   the tick. The daemon is still scanning / initializing.
/// - `Cooldown`: the per-job ready-relative delay
///   (`ready_<job>_delay_secs`) hasn't elapsed yet. The daemon reached
///   `Ready` recently but not long enough ago for this cron to fire.
/// - `LockBusy`: `heavy_cron_lock.try_lock()` lost the race to another
///   heavy cron. Six of seven heavy crons will skip this way each tick
///   while one runs.
/// - `Shutdown`: the `is_stopping()` check fired between scheduler
///   enqueue and worker dequeue. Avoids racing the closing PG pool /
///   broadcast channels during SIGTERM and demotes the resulting
///   "closed pool" / "disconnected channel" errors out of the log. See
///   plan ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
///   F3.
/// - `DbDown`: the `crate::health` DB-availability breaker reports the
///   database unreachable. The cron skips quietly (the prober owns the
///   single "database unreachable" log line) instead of stalling on a 10 s
///   `acquire_timeout` and logging an error every tick. See `crate::health`.
/// - `DiskPressure`: the `crate::health` disk watchdog has paused pgmcp's
///   disk-growing work because a watched filesystem is below its free-bytes
///   or free-inode floor. Applies to heavy crons (which write/grow data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    PhaseGate,
    Cooldown,
    LockBusy,
    Shutdown,
    DbDown,
    DiskPressure,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::PhaseGate => "phase_gate",
            SkipReason::Cooldown => "cooldown",
            SkipReason::LockBusy => "lock_busy",
            SkipReason::Shutdown => "shutdown",
            SkipReason::DbDown => "db_down",
            SkipReason::DiskPressure => "disk_pressure",
        }
    }
}

/// Outcome of the most recent run of a named cron job.
///
/// - `Ok`: closure entered the work body and the body completed
///   (whether or not it did N units of work — `<job>_runs` counters
///   track that separately at the top of each body).
/// - `NoOp`: closure entered the work body but the body's empty-data
///   path returned immediately (e.g. `max_chunk_id == 0`, no
///   projects, no embeddings yet). Distinguishes "scan ran, nothing
///   to do" from "scan never ran".
/// - `Skipped(reason)`: closure returned at one of the three gates
///   before entering the body. See [`SkipReason`].
/// - `Panicked`: anything `catch_unwind` caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronJobOutcome {
    Ok,
    NoOp,
    Skipped(SkipReason),
    Panicked,
}

impl CronJobOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CronJobOutcome::Ok => "ok",
            CronJobOutcome::NoOp => "no_op",
            CronJobOutcome::Skipped(SkipReason::PhaseGate) => "skipped:phase_gate",
            CronJobOutcome::Skipped(SkipReason::Cooldown) => "skipped:cooldown",
            CronJobOutcome::Skipped(SkipReason::LockBusy) => "skipped:lock_busy",
            CronJobOutcome::Skipped(SkipReason::Shutdown) => "skipped:shutdown",
            CronJobOutcome::Skipped(SkipReason::DbDown) => "skipped:db_down",
            CronJobOutcome::Skipped(SkipReason::DiskPressure) => "skipped:disk_pressure",
            CronJobOutcome::Panicked => "panicked",
        }
    }
}

/// Last-known status of one named cron job. Kept in the `last_cron_outcomes`
/// DashMap on `StatsTracker`; exposed via the JSON snapshot so dashboards
/// can distinguish "running cleanly", "panicked recently", and "never run"
/// per job rather than only seeing global `cron_panics`.
#[derive(Debug, Clone)]
pub struct CronJobStatus {
    pub outcome: CronJobOutcome,
    pub at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Monotonic write generation (from `StatsTracker::cron_outcome_seq`),
    /// stamped on every `record_cron_outcome`. The scheduler's `execute_inline`
    /// compares a job's seq before and after running its closure to tell whether
    /// the closure recorded its *own* terminal outcome (e.g. an inline gate
    /// skip) — if so, the scheduler must NOT clobber it with a default `Ok`.
    /// Not serialized (the JSON snapshot is built by hand from the other fields).
    pub seq: u64,
}
