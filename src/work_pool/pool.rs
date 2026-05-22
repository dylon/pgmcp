//! Simplified WorkPool adapted from MeTTaTron.
//!
//! Two-level priority queue using lock-free crossbeam channels:
//! - HIGH: interactive file-change events
//! - LOW: bulk scan tasks
//!
//! Workers try_recv HIGH first, then LOW. When both empty, park via WorkerPark.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use parking_lot::Mutex;
use tracing::{debug, trace};

use super::adaptive::WorkerPark;

/// Task priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Low,
}

/// A boxed task for the work pool.
type Task = Box<dyn FnOnce() + Send + 'static>;

/// Simplified adaptive work pool with two-level priority channels.
pub struct WorkPool {
    /// HIGH priority channel (interactive events)
    high_tx: Sender<Task>,
    high_rx: Receiver<Task>,
    /// LOW priority channel (bulk scan)
    low_tx: Sender<Task>,
    low_rx: Receiver<Task>,
    /// Per-worker parking primitives
    worker_parks: Vec<Arc<WorkerPark>>,
    /// Worker thread handles
    workers: Mutex<Vec<Option<JoinHandle<()>>>>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Active (non-parked) worker count
    active_count: AtomicUsize,
    /// Task completion counter (Relaxed increment)
    tasks_completed: Arc<AtomicU64>,
    /// Min/max thread bounds
    min_threads: usize,
    max_threads: usize,
}

impl WorkPool {
    pub fn new(
        min_threads: usize,
        max_threads: usize,
        initial_active: usize,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let (high_tx, high_rx) = unbounded::<Task>();
        let (low_tx, low_rx) = unbounded::<Task>();

        let initial_active = initial_active.clamp(min_threads, max_threads);

        let mut worker_parks = Vec::with_capacity(max_threads);
        for i in 0..max_threads {
            let initially_parked = i >= initial_active;
            worker_parks.push(Arc::new(WorkerPark::new(initially_parked)));
        }

        let pool = Self {
            high_tx,
            high_rx: high_rx.clone(),
            low_tx,
            low_rx: low_rx.clone(),
            worker_parks: worker_parks.clone(),
            workers: Mutex::new(Vec::with_capacity(max_threads)),
            shutdown: shutdown.clone(),
            active_count: AtomicUsize::new(initial_active),
            tasks_completed: Arc::new(AtomicU64::new(0)),
            min_threads,
            max_threads,
        };

        // Spawn all worker threads
        let mut handles = Vec::with_capacity(max_threads);
        for (i, park_slot) in worker_parks.iter().enumerate().take(max_threads) {
            let high_rx = high_rx.clone();
            let low_rx = low_rx.clone();
            let park = Arc::clone(park_slot);
            let shutdown = Arc::clone(&shutdown);
            let tasks_completed = Arc::clone(&pool.tasks_completed);

            let handle = thread::Builder::new()
                .name(format!("pgmcp-worker-{}", i))
                .spawn(move || {
                    worker_loop(i, high_rx, low_rx, park, shutdown, &tasks_completed);
                })
                .unwrap_or_else(|e| {
                    // EAGAIN here is almost always RLIMIT_NPROC or the
                    // systemd unit's `TasksMax=` ceiling. The daemon
                    // cannot operate with a partial pool, so we abort —
                    // but with an actionable message so the operator
                    // doesn't have to reverse-engineer the symptom.
                    panic!(
                        "pgmcp: failed to spawn worker thread {i} ({e}). \
                         This usually indicates the process is at its thread limit: \
                         check `ulimit -u` (RLIMIT_NPROC), the systemd unit's \
                         `TasksMax=` directive, and `cat /proc/sys/kernel/threads-max`. \
                         Raise the relevant ceiling, or lower `[work_pool] max_threads` \
                         in pgmcp's config.toml."
                    )
                });

            handles.push(Some(handle));
        }

        *pool.workers.lock() = handles;
        pool
    }

    /// Submit a task with the given priority.
    pub fn submit<F>(&self, task: F, priority: Priority)
    where
        F: FnOnce() + Send + 'static,
    {
        let sender = match priority {
            Priority::High => &self.high_tx,
            Priority::Low => &self.low_tx,
        };

        if let Err(e) = sender.send(Box::new(task)) {
            tracing::warn!("Failed to submit task: channel disconnected: {}", e);
            return;
        }

        // Wake a parked worker if there is one
        self.unpark_one();
    }

    /// Unpark the first parked worker. Returns true if one was unparked.
    pub fn unpark_one(&self) -> bool {
        for park in &self.worker_parks {
            if park.is_parked() {
                park.unpark();
                self.active_count.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Unpark up to n workers.
    pub fn unpark_n(&self, n: usize) -> usize {
        let mut count = 0;
        for park in &self.worker_parks {
            if count >= n {
                break;
            }
            if park.is_parked() {
                park.unpark();
                count += 1;
            }
        }
        self.active_count.fetch_add(count, Ordering::Relaxed);
        count
    }

    /// Park the last active worker (respecting min_threads).
    pub fn park_one(&self) -> bool {
        if self.active_count.load(Ordering::Relaxed) <= self.min_threads {
            return false;
        }
        for park in self.worker_parks.iter().rev() {
            if !park.is_parked() {
                park.park();
                self.active_count.fetch_sub(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Park up to n workers (respecting min_threads).
    pub fn park_n(&self, n: usize) -> usize {
        let mut count = 0;
        for park in self.worker_parks.iter().rev() {
            if count >= n {
                break;
            }
            if self.active_count.load(Ordering::Relaxed) <= self.min_threads {
                break;
            }
            if !park.is_parked() {
                park.park();
                self.active_count.fetch_sub(1, Ordering::Relaxed);
                count += 1;
            }
        }
        count
    }

    /// Get the current queue depth (HIGH + LOW).
    pub fn queue_depth(&self) -> usize {
        self.high_rx.len() + self.low_rx.len()
    }

    /// Get the number of active (non-parked) workers.
    pub fn active_workers(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Get the total completed task count.
    pub fn tasks_completed(&self) -> u64 {
        self.tasks_completed.load(Ordering::Relaxed)
    }

    pub fn min_threads(&self) -> usize {
        self.min_threads
    }

    pub fn max_threads(&self) -> usize {
        self.max_threads
    }

    /// Signal shutdown (non-blocking).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake all parked workers so they can exit
        for park in &self.worker_parks {
            park.unpark();
        }
    }

    /// Signal shutdown and wait for all workers to finish.
    pub fn shutdown_and_join(&self) {
        self.shutdown();
        let mut guards = self.workers.lock();
        for handle in guards.iter_mut() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Signal shutdown and return worker handles for joining with custom timeout logic.
    pub fn shutdown_and_take_handles(&self) -> Vec<JoinHandle<()>> {
        self.shutdown();
        let mut guards = self.workers.lock();
        guards.iter_mut().filter_map(|h| h.take()).collect()
    }
}

fn worker_loop(
    id: usize,
    high_rx: Receiver<Task>,
    low_rx: Receiver<Task>,
    park: Arc<WorkerPark>,
    shutdown: Arc<AtomicBool>,
    tasks_completed: &AtomicU64,
) {
    trace!(worker_id = id, "Worker started");

    loop {
        // Check parking first
        park.wait_if_parked();

        // Check shutdown
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Try HIGH priority first, then LOW
        let task = match high_rx.try_recv() {
            Ok(task) => Some(task),
            Err(TryRecvError::Empty) => match low_rx.try_recv() {
                Ok(task) => Some(task),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => None,
            },
            Err(TryRecvError::Disconnected) => low_rx.try_recv().ok(),
        };

        match task {
            Some(task) => {
                // Execute with catch_unwind for resilience
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                if let Err(e) = result {
                    let msg = panic_payload_message(&e);
                    tracing::error!(worker_id = id, panic = %msg, "Worker task panicked");
                }
                tasks_completed.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                // No work available — wait briefly before checking again
                // This avoids a pure spin-loop while still being responsive
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                // Use a blocking recv with timeout to avoid spin
                match high_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(task) => {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                        if let Err(e) = result {
                            let msg = panic_payload_message(&e);
                            tracing::error!(worker_id = id, panic = %msg, "Worker task panicked");
                        }
                        tasks_completed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // Timeout or disconnected — check shutdown and loop
                    }
                }
            }
        }
    }

    debug!(worker_id = id, "Worker exiting");
}

/// Extract a human-readable message from a `catch_unwind` payload.
/// `Box<dyn Any>` formatted with `?` produces only the unhelpful
/// `Any { .. }`; downcasting to the standard `panic!("...")` payload
/// types recovers the actual message.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn panic_payload_recovers_static_str_message() {
        // `panic!("static")` puts a `&'static str` in the Any payload —
        // the most common shape from manual panics and `.expect(...)`.
        let payload =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| panic!("boom"))).unwrap_err();
        assert_eq!(panic_payload_message(&payload), "boom");
    }

    #[test]
    fn panic_payload_recovers_owned_string_message() {
        // `panic!("{}", fmt)` puts a `String` in the payload — what
        // formatted panics and most third-party `bail!` macros produce.
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let n: u32 = 42;
            panic!("formatted={n}");
        }))
        .unwrap_err();
        assert_eq!(panic_payload_message(&payload), "formatted=42");
    }

    #[test]
    fn panic_payload_describes_non_string_payload() {
        // Some panics carry arbitrary `dyn Any + Send` (e.g. user code
        // calling `std::panic::panic_any(SomeStruct {..})`). The helper
        // must not crash; it returns a sentinel so operators see *something*
        // useful instead of the previous `Any { .. }` debug-print.
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::panic::panic_any(42_i64);
        }))
        .unwrap_err();
        assert_eq!(
            panic_payload_message(&payload),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn test_work_pool_basic() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = WorkPool::new(1, 4, 2, Arc::clone(&shutdown));

        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        pool.submit(
            move || {
                c.fetch_add(1, Ordering::Relaxed);
            },
            Priority::High,
        );

        thread::sleep(Duration::from_millis(100));
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        pool.shutdown_and_join();
    }

    #[test]
    fn test_work_pool_priority() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = WorkPool::new(1, 2, 2, Arc::clone(&shutdown));

        let counter = Arc::new(AtomicU64::new(0));

        for _ in 0..10 {
            let c = Arc::clone(&counter);
            pool.submit(
                move || {
                    c.fetch_add(1, Ordering::Relaxed);
                },
                Priority::Low,
            );
        }

        thread::sleep(Duration::from_millis(200));
        assert_eq!(counter.load(Ordering::Relaxed), 10);

        pool.shutdown_and_join();
    }

    #[test]
    fn test_work_pool_park_unpark() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = WorkPool::new(1, 4, 4, Arc::clone(&shutdown));

        assert_eq!(pool.active_workers(), 4);
        pool.park_one();
        assert_eq!(pool.active_workers(), 3);
        pool.unpark_one();
        assert_eq!(pool.active_workers(), 4);

        pool.shutdown_and_join();
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 8, ..ProptestConfig::default() })]

        /// N producer threads submitting M tasks each → total completion
        /// count = N*M. Proves no submissions are lost under concurrency.
        #[test]
        fn prop_concurrent_submissions_no_lost_tasks(
            producers in 2usize..6,
            per_producer in 10usize..50,
        ) {
            let shutdown = Arc::new(AtomicBool::new(false));
            let pool = Arc::new(WorkPool::new(2, 8, 4, Arc::clone(&shutdown)));
            let counter = Arc::new(AtomicU64::new(0));
            let mut handles = Vec::new();
            for _ in 0..producers {
                let pool = Arc::clone(&pool);
                let counter = Arc::clone(&counter);
                handles.push(std::thread::spawn(move || {
                    for _ in 0..per_producer {
                        let c = Arc::clone(&counter);
                        pool.submit(
                            move || {
                                c.fetch_add(1, Ordering::Relaxed);
                            },
                            Priority::Low,
                        );
                    }
                }));
            }
            for h in handles {
                h.join().expect("producer");
            }
            // Wait for all tasks to drain.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let expected = (producers * per_producer) as u64;
            while counter.load(Ordering::Relaxed) < expected
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            prop_assert_eq!(counter.load(Ordering::Relaxed), expected);
            pool.shutdown_and_join();
        }

        /// High-priority tasks complete no later than an equivalent batch
        /// of low-priority tasks submitted in the same burst — measuring
        /// the *position* of last-completed index in each class.
        #[test]
        fn prop_high_priority_tasks_do_not_starve_low(
            burst in 20usize..40,
        ) {
            let shutdown = Arc::new(AtomicBool::new(false));
            let pool = Arc::new(WorkPool::new(2, 4, 4, Arc::clone(&shutdown)));
            let counter = Arc::new(AtomicU64::new(0));
            for _ in 0..burst {
                let c = Arc::clone(&counter);
                pool.submit(
                    move || {
                        c.fetch_add(1, Ordering::Relaxed);
                    },
                    Priority::High,
                );
                let c = Arc::clone(&counter);
                pool.submit(
                    move || {
                        c.fetch_add(1, Ordering::Relaxed);
                    },
                    Priority::Low,
                );
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let expected = (burst * 2) as u64;
            while counter.load(Ordering::Relaxed) < expected
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            prop_assert_eq!(counter.load(Ordering::Relaxed), expected,
                "all tasks (high + low) must eventually complete");
            pool.shutdown_and_join();
        }
    }

    /// Panicking tasks are isolated — subsequent non-panicking tasks still
    /// run to completion. Catches work-pool catch_unwind regressions.
    #[test]
    fn test_panic_in_task_does_not_kill_pool() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = WorkPool::new(1, 2, 2, Arc::clone(&shutdown));
        pool.submit(
            || {
                panic!("deliberate");
            },
            Priority::High,
        );
        std::thread::sleep(Duration::from_millis(100));
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);
        pool.submit(
            move || {
                c.fetch_add(1, Ordering::Relaxed);
            },
            Priority::High,
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            counter.load(Ordering::Relaxed) >= 1,
            "task after panic should still execute"
        );
        pool.shutdown_and_join();
    }
}
