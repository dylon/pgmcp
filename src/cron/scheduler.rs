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
use crate::stats::tracker::StatsTracker;

// ============================================================================
// Time utilities
// ============================================================================

pub type UnixTimestampMs = u64;

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

#[inline]
pub fn now_ms() -> UnixTimestampMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time went backwards")
        .as_millis() as u64
}

// ============================================================================
// CronState
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronState {
    CheckEvents,
    DrainChannel,
    ExecutingTask,
    Sleeping,
    Terminated,
}

// ============================================================================
// CronEvent
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronEvent {
    TaskReceived,
    TimerExpired,
    TaskDue,
    TaskCompleted { success: bool, should_requeue: bool },
    TerminationRequested,
    ChannelDisconnected,
    NoEvents,
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (task.task)()));

        if let Some(s) = stats {
            s.cron_executions.fetch_add(1, AtomicOrdering::Relaxed);
        }

        match result {
            Ok(true) => {
                if let Some(interval) = task.metadata.recurrence_interval() {
                    task.scheduled_time_ms = now_ms() + interval;
                    queue.push(task);
                }
            }
            Ok(false) => {}
            Err(e) => {
                error!(task_name = task.metadata.name(), panic = ?e, "Task panicked");
                if let Some(s) = stats {
                    s.cron_panics.fetch_add(1, AtomicOrdering::Relaxed);
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
    rt: tokio::runtime::Handle,
    embed_tx: crossbeam_channel::Sender<EmbedIndexRequest>,
    lifecycle: DaemonLifecycle,
    cron_pool: Arc<crate::work_pool::pool::WorkPool>,
    general_pool: Option<Arc<crate::work_pool::pool::WorkPool>>,
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
        1_000, // 1s base delay — the real wait happens on Ready-relative check below
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
                    if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
                        return;
                    }
                    let first_seen = ready_git.get_or_init(Instant::now);
                    if first_seen.elapsed() < git_ready_delay {
                        return; // still within post-Ready cooldown
                    }
                    let _guard = match lock.try_lock() {
                        Some(g) => g,
                        None => {
                            tracing::info!("heavy cron busy, deferring git-history-index");
                            return;
                        }
                    };
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_git));
                    let stats = Arc::clone(&stats_for_git);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        match db.get_git_enabled_projects().await {
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
    handle.schedule_recurring(1_000, sim_interval * 1000, "similarity-scan", move || {
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
                if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
                    return;
                }
                let first_seen = ready_sim.get_or_init(Instant::now);
                if first_seen.elapsed() < sim_ready_delay {
                    return;
                }
                let _guard = match lock.try_lock() {
                    Some(g) => g,
                    None => {
                        tracing::info!("heavy cron busy, deferring similarity-scan");
                        return;
                    }
                };
                let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_sim));
                let stats = Arc::clone(&stats_for_sim);
                let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                let t0 = Instant::now();
                rt.block_on(async {
                    crate::cron::similarity::run_similarity_scan(
                        db.as_ref(),
                        &cfg,
                        sim_ef_search,
                        &stats,
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
    });

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
    handle.schedule_recurring(1_000, graph_interval * 1000, "graph-analysis", move || {
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
                if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
                    return;
                }
                let first_seen = ready_graph.get_or_init(Instant::now);
                if first_seen.elapsed() < graph_ready_delay {
                    return;
                }
                let _guard = match lock.try_lock() {
                    Some(g) => g,
                    None => {
                        tracing::info!("heavy cron busy, deferring graph-analysis");
                        return;
                    }
                };
                let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_graph));
                let stats = Arc::clone(&stats_for_graph);
                let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                let t0 = Instant::now();
                rt.block_on(async {
                    crate::cron::graph_analysis::run_graph_analysis(db.as_ref(), &stats, wp).await;
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
    });

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
        1_000,
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
                    if !lc.is_at_least(crate::daemon_state::DaemonPhase::Ready) {
                        return;
                    }
                    let first_seen = ready_topic.get_or_init(Instant::now);
                    if first_seen.elapsed() < topic_ready_delay {
                        return;
                    }
                    let _guard = match lock.try_lock() {
                        Some(g) => g,
                        None => {
                            tracing::info!("heavy cron busy, deferring topic-clustering");
                            return;
                        }
                    };
                    let _cron_flag = HeavyCronFlag::new(Arc::clone(&stats_for_topic));
                    let stats = Arc::clone(&stats_for_topic);
                    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                    let t0 = Instant::now();
                    rt.block_on(async {
                        crate::cron::topic_clustering::run_global_topic_scan(
                            db.as_ref(),
                            &cfg,
                            &stats,
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
    use std::time::Instant;

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

        handle.schedule_once(0, "panicking", || {
            panic!("intentional panic");
        });
        handle.schedule_once(50, "normal", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            if Instant::now() > deadline {
                panic!("Timeout waiting for normal task");
            }
            thread::sleep(Duration::from_millis(10));
        }

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
}
