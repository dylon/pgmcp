//! Scaling monitor for a WorkPool instance.
//!
//! Four-term objective:
//!   J(N) = -w_tp * ema_tp
//!         +  w_qd * ema_qd
//!         +  w_mp * ema_slab
//!         +  w_rss * ema_rss
//!
//! `ema_slab` is currently always 0 (pgmcp has no slab/GC allocator); the
//! term is wired so a future allocator backpressure signal can plug in
//! without re-shaping the objective. `ema_rss` is the
//! `rss_pressure_score` from `super::adaptive` clamped to `[0.0, 3.0]`.
//!
//! Each pool (InferencePool's embed_pool counterpart, CronPool, GeneralPool)
//! gets its own monitor running on a dedicated thread at 200 ms intervals
//! with its own RSS budget. When RSS climbs toward the budget, the
//! monitor's hill climber parks workers preemptively — that's the
//! load-bearing safety net behind every memory hotspot the rescan/flood
//! scenario exposed earlier.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tracing::trace;

use super::adaptive::{Ema, HillClimber, ScaleAction, rss_pressure_score};
use super::pool::WorkPool;
use crate::stats::tracker::StatsTracker;

/// Monitor interval in milliseconds.
const MONITOR_INTERVAL_MS: u64 = 200;

/// EMA smoothing factor (half-life ~4.3 samples = ~860ms).
const EMA_ALPHA: f64 = 0.15;

/// Cooldown period (ticks) after a scaling action.
const COOLDOWN_PERIOD: u32 = 5;

/// Minimum improvement threshold.
const IMPROVEMENT_THRESHOLD: f64 = 0.05;

/// Throughput weight (negative coefficient = maximize).
const THROUGHPUT_WEIGHT: f64 = 1.0;

/// Queue depth weight (positive coefficient = minimize).
const QUEUE_DEPTH_WEIGHT: f64 = 2.0;

/// Memory-pressure weight (slab/GC backpressure). Currently always 0 in
/// pgmcp because no GC allocator is in use; reserved for forward-compat.
const MEMORY_PRESSURE_WEIGHT: f64 = 5.0;

/// RSS-pressure weight (highest-priority signal — direct OOM avoidance).
const RSS_PRESSURE_WEIGHT: f64 = 8.0;

/// Run the scaling monitor loop on the current thread.
///
/// `rss_limit_bytes = 0` disables the RSS term (the climber falls back to
/// the original two-term throughput-vs-queue-depth behavior).
pub fn run_scaling_monitor(
    pool: &WorkPool,
    shutdown: Arc<AtomicBool>,
    stats: &StatsTracker,
    rss_limit_bytes: u64,
) {
    let mut ema_throughput = Ema::new(EMA_ALPHA);
    let mut ema_queue_depth = Ema::new(EMA_ALPHA);
    let mut ema_rss = Ema::new(EMA_ALPHA);
    let mut climber = HillClimber::new(
        COOLDOWN_PERIOD,
        IMPROVEMENT_THRESHOLD,
        pool.min_threads(),
        pool.max_threads(),
        pool.active_workers(),
    );

    let mut prev_completed = pool.tasks_completed();

    while !shutdown.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(MONITOR_INTERVAL_MS));

        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Sample throughput (tasks completed since last tick)
        let completed = pool.tasks_completed();
        let throughput = (completed - prev_completed) as f64;
        prev_completed = completed;

        // Sample queue depth
        let queue_depth = pool.queue_depth() as f64;

        // Sample RSS pressure (0.0 if disabled)
        let rss_p = rss_pressure_score(rss_limit_bytes);

        // Update EMAs
        let tp = ema_throughput.update(throughput);
        let qd = ema_queue_depth.update(queue_depth);
        let rp = ema_rss.update(rss_p);
        let mp = 0.0_f64; // pgmcp has no slab/GC allocator yet

        // Compute four-term objective (lower = better).
        let objective = -THROUGHPUT_WEIGHT * tp
            + QUEUE_DEPTH_WEIGHT * qd
            + MEMORY_PRESSURE_WEIGHT * mp
            + RSS_PRESSURE_WEIGHT * rp;

        // Feed to hill climber
        let decision = climber.step(objective);

        match decision.action {
            ScaleAction::Unpark => {
                let actual = pool.unpark_n(decision.count);
                trace!(
                    requested = decision.count,
                    actual, "Scaling monitor: unpark"
                );
                stats.work_pool_scale_ups.fetch_add(1, Ordering::Relaxed);
            }
            ScaleAction::Park => {
                let actual = pool.park_n(decision.count);
                trace!(requested = decision.count, actual, "Scaling monitor: park");
                stats.work_pool_scale_downs.fetch_add(1, Ordering::Relaxed);
            }
            ScaleAction::Hold => {}
        }

        // Update stats tracker
        stats
            .active_work_pool_threads
            .store(pool.active_workers() as u64, Ordering::Relaxed);
        stats
            .work_pool_queue_depth
            .store(pool.queue_depth() as u64, Ordering::Relaxed);
        stats
            .work_pool_tasks_completed
            .store(completed, Ordering::Relaxed);
    }
}
