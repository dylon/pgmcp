//! Shared Abstractions for Adaptive Thread Pools
//!
//! Adapted from MeTTaTron's adaptive_pool.rs. Provides:
//!
//! - **`Ema`** — Exponential moving average with configurable smoothing factor
//! - **`HillClimber`** — ±1 perturbation-based hill climbing optimizer
//! - **`ScaleAction`** — Decision enum for thread scaling actions
//! - **`WorkerPark`** — Per-worker parking primitive (Mutex<bool> + Condvar)

use parking_lot::{Condvar, Mutex};
use std::time::Duration;

// ============================================================================
// Exponential Moving Average (EMA)
// ============================================================================

/// Exponential moving average with configurable smoothing factor.
///
/// The EMA update formula is:
/// ```text
/// ema = alpha * sample + (1 - alpha) * ema
/// ```
///
/// Where alpha in (0, 1] controls responsiveness:
/// - Higher alpha -> more responsive to recent values, noisier
/// - Lower alpha -> smoother, more lag
///
/// Half-life in samples: `ln(2) / ln(1 / (1 - alpha))`
/// For alpha=0.15: half-life ~ 4.3 samples
#[derive(Debug, Clone)]
pub struct Ema {
    alpha: f64,
    value: f64,
    initialized: bool,
}

impl Ema {
    /// Create a new EMA with the given smoothing factor.
    ///
    /// # Panics
    /// Panics if `alpha` is not in (0, 1].
    pub fn new(alpha: f64) -> Self {
        assert!(
            alpha > 0.0 && alpha <= 1.0,
            "EMA alpha must be in (0, 1], got {}",
            alpha
        );
        Self {
            alpha,
            value: 0.0,
            initialized: false,
        }
    }

    /// Update the EMA with a new sample and return the updated value.
    ///
    /// The first sample initializes the EMA to that value (no lag).
    #[inline]
    pub fn update(&mut self, sample: f64) -> f64 {
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        }
        self.value
    }

    /// Get the current EMA value.
    #[inline]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Check if the EMA has been initialized with at least one sample.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Reset the EMA to uninitialized state.
    pub fn reset(&mut self) {
        self.value = 0.0;
        self.initialized = false;
    }
}

// ============================================================================
// Scale Action
// ============================================================================

/// Decision produced by the hill climber for thread scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleAction {
    /// Unpark (activate) worker thread(s).
    Unpark,
    /// Park (deactivate) worker thread(s).
    Park,
    /// No change.
    Hold,
}

/// Scaling decision with the number of workers to park/unpark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleDecision {
    pub action: ScaleAction,
    pub count: usize,
}

// ============================================================================
// Hill Climber
// ============================================================================

/// Geometric-step hill climber for adaptive thread pool sizing.
///
/// Minimizes the objective function: lower values = better.
///
/// Step size doubles on consecutive improvements in the same direction,
/// capped at `max_threads / 4`. On direction reversal, step size resets to 1.
#[derive(Debug, Clone)]
pub struct HillClimber {
    prev_objective: f64,
    direction: i32,
    cooldown_remaining: u32,
    cooldown_period: u32,
    improvement_threshold: f64,
    current_active: usize,
    min_threads: usize,
    max_threads: usize,
    initialized: bool,
    step_size: usize,
}

impl HillClimber {
    pub fn new(
        cooldown_period: u32,
        improvement_threshold: f64,
        min_threads: usize,
        max_threads: usize,
        initial_active: usize,
    ) -> Self {
        Self {
            prev_objective: 0.0,
            direction: 1,
            cooldown_remaining: 0,
            cooldown_period,
            improvement_threshold,
            current_active: initial_active,
            min_threads,
            max_threads,
            initialized: false,
            step_size: 1,
        }
    }

    /// Feed a new objective value and get the recommended scaling decision.
    ///
    /// The objective should be minimized (lower = better). Two-term formula:
    /// ```text
    /// J(N) = -w_tp * ema_tp + w_qd * ema_qd
    /// ```
    pub fn step(&mut self, objective: f64) -> ScaleDecision {
        let hold = ScaleDecision {
            action: ScaleAction::Hold,
            count: 0,
        };

        if !self.initialized {
            self.prev_objective = objective;
            self.initialized = true;
            return hold;
        }

        // During cooldown: hold and decrement. Keep prev_objective frozen.
        if self.cooldown_remaining > 0 {
            self.cooldown_remaining -= 1;
            return hold;
        }

        let improvement = self.prev_objective - objective;
        self.prev_objective = objective;

        if improvement >= self.improvement_threshold {
            self.step_size = self.accelerated_step_size();
            self.apply_direction()
        } else if improvement <= -self.improvement_threshold {
            self.direction = -self.direction;
            self.step_size = 1;
            self.apply_direction()
        } else {
            hold
        }
    }

    fn accelerated_step_size(&self) -> usize {
        let cap = (self.max_threads / 4).max(1);
        (self.step_size * 2).min(cap)
    }

    fn apply_direction(&mut self) -> ScaleDecision {
        if self.direction > 0 {
            let new = (self.current_active + self.step_size).min(self.max_threads);
            if new == self.current_active {
                return ScaleDecision {
                    action: ScaleAction::Hold,
                    count: 0,
                };
            }
            let count = new - self.current_active;
            self.current_active = new;
            self.cooldown_remaining = self.cooldown_period;
            ScaleDecision {
                action: ScaleAction::Unpark,
                count,
            }
        } else {
            let new = self
                .current_active
                .saturating_sub(self.step_size)
                .max(self.min_threads);
            if new == self.current_active {
                return ScaleDecision {
                    action: ScaleAction::Hold,
                    count: 0,
                };
            }
            let count = self.current_active - new;
            self.current_active = new;
            self.cooldown_remaining = self.cooldown_period;
            ScaleDecision {
                action: ScaleAction::Park,
                count,
            }
        }
    }

    #[inline]
    pub fn current_active(&self) -> usize {
        self.current_active
    }

    #[inline]
    pub fn direction(&self) -> i32 {
        self.direction
    }

    #[inline]
    pub fn cooldown_remaining(&self) -> u32 {
        self.cooldown_remaining
    }

    #[inline]
    pub fn prev_objective(&self) -> f64 {
        self.prev_objective
    }

    #[inline]
    pub fn improvement_threshold(&self) -> f64 {
        self.improvement_threshold
    }

    #[inline]
    pub fn step_size(&self) -> usize {
        self.step_size
    }

    pub fn set_current_active(&mut self, count: usize) {
        self.current_active = count.clamp(self.min_threads, self.max_threads);
    }
}

// ============================================================================
// WorkerPark
// ============================================================================

/// Per-worker parking primitive for adaptive thread pool workers.
pub struct WorkerPark {
    parked: Mutex<bool>,
    condvar: Condvar,
}

impl WorkerPark {
    pub fn new(initially_parked: bool) -> Self {
        Self {
            parked: Mutex::new(initially_parked),
            condvar: Condvar::new(),
        }
    }

    /// Park the worker (called by the scaling monitor).
    pub fn park(&self) {
        let mut parked = self.parked.lock();
        *parked = true;
    }

    /// Unpark the worker (called by the scaling monitor).
    pub fn unpark(&self) {
        let mut parked = self.parked.lock();
        *parked = false;
        self.condvar.notify_one();
    }

    /// Block until unparked. Returns immediately if not parked.
    pub fn wait_if_parked(&self) {
        let mut parked = self.parked.lock();
        while *parked {
            self.condvar.wait(&mut parked);
        }
    }

    pub fn is_parked(&self) -> bool {
        *self.parked.lock()
    }

    /// Wait if parked, with timeout.
    /// Returns `true` if unparked, `false` if timed out while still parked.
    pub fn wait_if_parked_timeout(&self, timeout: Duration) -> bool {
        let mut parked = self.parked.lock();
        if !*parked {
            return true;
        }
        let result = self.condvar.wait_for(&mut parked, timeout);
        if result.timed_out() {
            return !*parked;
        }
        !*parked
    }
}

// ============================================================================
// RSS-pressure helper
// ============================================================================

/// RSS pressure score in `[0.0, 3.0]`, used as a hill-climber input term
/// so each pool can self-throttle as it approaches its memory budget.
///
/// Linear ramp from 0.0 at 50% of the limit to 3.0 at 125% of the limit,
/// then clamped. Mirrors MeTTaTron's `rss_pressure` shape — the early
/// onset (50%) gives the climber room to react before peak; the
/// extra-budget tail (>100%) keeps the signal monotonic and saturating.
///
/// Returns 0.0 when `rss_limit_bytes == 0` (RSS sensing disabled) or when
/// `/proc/self/statm` is unreadable.
pub fn rss_pressure_score(rss_limit_bytes: u64) -> f64 {
    if rss_limit_bytes == 0 {
        return 0.0;
    }
    let Some(rss) = crate::stats::rss::current_rss_bytes() else {
        return 0.0;
    };
    let ratio = rss as f64 / rss_limit_bytes as f64;
    ((ratio - 0.5) * 4.0).clamp(0.0, 3.0)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn rss_pressure_score_returns_zero_when_limit_disabled() {
        // 0 = sensing disabled; result must be 0 regardless of RSS.
        assert_eq!(rss_pressure_score(0), 0.0);
    }

    #[test]
    fn rss_pressure_score_clamps_above_3_0() {
        // Use a deliberately tiny limit so the daemon's actual RSS is
        // far above it, forcing the upper clamp.
        let v = rss_pressure_score(1);
        assert!(
            (v - 3.0).abs() < 1e-9,
            "expected upper clamp at 3.0, got {v}"
        );
    }

    #[test]
    fn rss_pressure_score_clamps_below_zero() {
        // A limit so large that current RSS can't possibly be near 50% of
        // it — the score must clamp to 0, not go negative.
        let v = rss_pressure_score(u64::MAX / 2);
        assert!(v >= 0.0, "score must be non-negative; got {v}");
        assert!(v <= 3.0, "score must clamp at 3.0; got {v}");
    }

    #[test]
    fn test_ema_converges_to_steady_state() {
        let mut ema = Ema::new(0.15);
        for _ in 0..100 {
            ema.update(42.0);
        }
        assert!(
            (ema.value() - 42.0).abs() < 0.001,
            "EMA should converge to 42.0, got {}",
            ema.value()
        );
    }

    #[test]
    fn test_ema_first_sample_initializes() {
        let mut ema = Ema::new(0.5);
        assert!(!ema.is_initialized());
        let val = ema.update(100.0);
        assert_eq!(val, 100.0);
        assert!(ema.is_initialized());
    }

    #[test]
    fn test_ema_alpha_responsiveness() {
        let mut high = Ema::new(0.9);
        high.update(0.0);
        high.update(100.0);

        let mut low = Ema::new(0.1);
        low.update(0.0);
        low.update(100.0);

        assert!(high.value() > low.value());
    }

    #[test]
    fn test_ema_reset() {
        let mut ema = Ema::new(0.5);
        ema.update(100.0);
        ema.reset();
        assert!(!ema.is_initialized());
        assert_eq!(ema.value(), 0.0);
    }

    #[test]
    #[should_panic(expected = "EMA alpha must be in (0, 1]")]
    fn test_ema_rejects_zero_alpha() {
        Ema::new(0.0);
    }

    #[test]
    #[should_panic(expected = "EMA alpha must be in (0, 1]")]
    fn test_ema_rejects_negative_alpha() {
        Ema::new(-0.5);
    }

    #[test]
    fn test_hill_climber_first_step_holds() {
        let mut climber = HillClimber::new(3, 0.05, 1, 8, 4);
        let decision = climber.step(10.0);
        assert_eq!(decision.action, ScaleAction::Hold);
        assert_eq!(decision.count, 0);
    }

    #[test]
    fn test_hill_climber_improvement_continues_direction() {
        let mut climber = HillClimber::new(0, 0.05, 1, 8, 4);
        climber.step(10.0);
        let decision = climber.step(9.0);
        assert_eq!(decision.action, ScaleAction::Unpark);
        assert!(decision.count >= 1);
    }

    #[test]
    fn test_hill_climber_worsening_reverses_direction() {
        let mut climber = HillClimber::new(0, 0.05, 1, 8, 4);
        climber.step(10.0);
        let decision = climber.step(11.0);
        assert_eq!(decision.action, ScaleAction::Park);
        assert_eq!(decision.count, 1);
    }

    #[test]
    fn test_hill_climber_plateau_holds() {
        let mut climber = HillClimber::new(0, 0.05, 1, 8, 4);
        climber.step(10.0);
        let decision = climber.step(10.01);
        assert_eq!(decision.action, ScaleAction::Hold);
    }

    #[test]
    fn test_hill_climber_cooldown_behavior() {
        let mut climber = HillClimber::new(3, 0.05, 1, 8, 4);
        climber.step(10.0);
        let decision = climber.step(5.0);
        assert_eq!(decision.action, ScaleAction::Unpark);
        assert_eq!(climber.step(4.0).action, ScaleAction::Hold);
        assert_eq!(climber.step(3.0).action, ScaleAction::Hold);
        assert_eq!(climber.step(2.0).action, ScaleAction::Hold);
        let decision = climber.step(1.0);
        assert_eq!(decision.action, ScaleAction::Unpark);
    }

    #[test]
    fn test_hill_climber_max_boundary() {
        let mut climber = HillClimber::new(0, 0.05, 1, 4, 4);
        climber.step(10.0);
        let decision = climber.step(5.0);
        assert_eq!(decision.action, ScaleAction::Hold);
    }

    #[test]
    fn test_hill_climber_min_boundary() {
        let mut climber = HillClimber::new(0, 0.05, 4, 8, 4);
        climber.step(10.0);
        let decision = climber.step(15.0);
        assert_eq!(decision.action, ScaleAction::Hold);
    }

    #[test]
    fn test_hill_climber_geometric_acceleration() {
        let mut climber = HillClimber::new(0, 0.05, 1, 64, 1);
        climber.step(100.0);

        let d1 = climber.step(90.0);
        assert_eq!(d1.action, ScaleAction::Unpark);
        assert_eq!(d1.count, 2);
        assert_eq!(climber.current_active(), 3);

        let d2 = climber.step(80.0);
        assert_eq!(d2.action, ScaleAction::Unpark);
        assert_eq!(d2.count, 4);
        assert_eq!(climber.current_active(), 7);

        let d3 = climber.step(70.0);
        assert_eq!(d3.action, ScaleAction::Unpark);
        assert_eq!(d3.count, 8);
        assert_eq!(climber.current_active(), 15);

        let d4 = climber.step(60.0);
        assert_eq!(d4.action, ScaleAction::Unpark);
        assert_eq!(d4.count, 16);
        assert_eq!(climber.current_active(), 31);

        let d5 = climber.step(200.0);
        assert_eq!(d5.action, ScaleAction::Park);
        assert_eq!(d5.count, 1);
        assert_eq!(climber.current_active(), 30);
    }

    #[test]
    fn test_worker_park_initially_unparked() {
        let wp = WorkerPark::new(false);
        assert!(!wp.is_parked());
    }

    #[test]
    fn test_worker_park_initially_parked() {
        let wp = WorkerPark::new(true);
        assert!(wp.is_parked());
    }

    #[test]
    fn test_worker_park_and_unpark() {
        let wp = WorkerPark::new(false);
        wp.park();
        assert!(wp.is_parked());
        wp.unpark();
        assert!(!wp.is_parked());
    }

    #[test]
    fn test_worker_park_blocks_thread() {
        let wp = Arc::new(WorkerPark::new(true));
        let wp_clone = Arc::clone(&wp);
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);

        let handle = thread::spawn(move || {
            wp_clone.wait_if_parked();
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        thread::sleep(Duration::from_millis(50));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);

        wp.unpark();
        handle.join().expect("Thread panicked");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_worker_park_not_parked_returns_immediately() {
        let wp = WorkerPark::new(false);
        wp.wait_if_parked();
    }

    #[test]
    fn test_worker_park_timeout() {
        let wp = WorkerPark::new(true);
        let start = std::time::Instant::now();
        let result = wp.wait_if_parked_timeout(Duration::from_millis(50));
        let elapsed = start.elapsed();
        assert!(!result);
        assert!(elapsed >= Duration::from_millis(40));
    }

    // ========================================================================
    // Proptest: EMA
    // ========================================================================

    use proptest::prelude::*;

    /// Strategy for valid EMA alpha values in (0, 1].
    fn alpha_strategy() -> impl Strategy<Value = f64> {
        (1u32..=1000).prop_map(|n| n as f64 / 1000.0)
    }

    proptest! {
        #[test]
        fn prop_ema_bounded_between_min_and_max(
            alpha in alpha_strategy(),
            samples in prop::collection::vec(-1000.0f64..1000.0, 1..200),
        ) {
            let mut ema = Ema::new(alpha);
            let mut min_sample = f64::INFINITY;
            let mut max_sample = f64::NEG_INFINITY;

            for &s in &samples {
                ema.update(s);
                min_sample = min_sample.min(s);
                max_sample = max_sample.max(s);
            }

            // EMA value must be within the range of observed samples
            let val = ema.value();
            prop_assert!(
                val >= min_sample - f64::EPSILON && val <= max_sample + f64::EPSILON,
                "EMA value {} should be between {} and {}",
                val, min_sample, max_sample
            );
        }

        #[test]
        fn prop_ema_constant_input_converges(
            alpha in alpha_strategy(),
            constant in -1000.0f64..1000.0,
            n in 10usize..100,
        ) {
            let mut ema = Ema::new(alpha);
            for _ in 0..n {
                ema.update(constant);
            }

            let diff = (ema.value() - constant).abs();
            prop_assert!(
                diff < 1e-6,
                "EMA should converge to constant {}, got {}, diff={}",
                constant, ema.value(), diff
            );
        }

        #[test]
        fn prop_ema_first_sample_sets_value(
            alpha in alpha_strategy(),
            first in -1000.0f64..1000.0,
        ) {
            let mut ema = Ema::new(alpha);
            let val = ema.update(first);
            prop_assert_eq!(val, first, "first sample must set EMA value");
            prop_assert!(ema.is_initialized());
        }
    }

    // ========================================================================
    // Proptest: HillClimber
    // ========================================================================

    proptest! {
        #[test]
        fn prop_hill_climber_stays_within_bounds(
            objectives in prop::collection::vec(-100.0f64..100.0, 2..100),
            min_threads in 1usize..4,
            initial_extra in 0usize..8,
            max_extra in 0usize..60,
        ) {
            let initial = min_threads + initial_extra;
            let max_threads = (initial + max_extra).max(initial);

            let mut climber = HillClimber::new(0, 0.01, min_threads, max_threads, initial);

            for &obj in &objectives {
                climber.step(obj);
                let active = climber.current_active();
                prop_assert!(
                    active >= min_threads && active <= max_threads,
                    "current_active {} must be in [{}, {}]",
                    active, min_threads, max_threads
                );
            }
        }

        #[test]
        fn prop_hill_climber_geometric_step_capped(
            improvements in prop::collection::vec(1.0f64..50.0, 2..50),
            max_threads in 8usize..128,
        ) {
            let min_threads = 1;
            let mut climber = HillClimber::new(0, 0.01, min_threads, max_threads, min_threads);

            // Feed monotonically decreasing objectives (continuous improvement)
            let mut obj = 100.0;
            climber.step(obj);
            for &delta in &improvements {
                obj -= delta;
                let decision = climber.step(obj);
                // Step size cap: max_threads / 4
                let cap = (max_threads / 4).max(1);
                prop_assert!(
                    decision.count <= cap,
                    "step count {} exceeded cap {} (max_threads={})",
                    decision.count, cap, max_threads
                );
            }
        }

        #[test]
        fn prop_hill_climber_first_step_always_holds(
            objective in -100.0f64..100.0,
            min in 1usize..4,
            max in 4usize..32,
        ) {
            let max = max.max(min);
            let initial = min;
            let mut climber = HillClimber::new(3, 0.05, min, max, initial);
            let decision = climber.step(objective);
            prop_assert_eq!(decision.action, ScaleAction::Hold);
            prop_assert_eq!(decision.count, 0);
        }

        #[test]
        fn prop_hill_climber_cooldown_holds(
            cooldown in 1u32..10,
            obj1 in -50.0f64..50.0,
            obj2 in -50.0f64..50.0,
        ) {
            // Ensure obj2 is significantly different from obj1 to trigger scaling
            let obj2 = if (obj1 - obj2).abs() < 1.0 { obj1 + 5.0 } else { obj2 };

            let mut climber = HillClimber::new(cooldown, 0.05, 1, 32, 8);
            climber.step(obj1);
            let first = climber.step(obj2);

            // If first step changed something, next `cooldown` steps must hold
            if first.action != ScaleAction::Hold {
                for _ in 0..cooldown {
                    let d = climber.step(0.0);
                    prop_assert_eq!(d.action, ScaleAction::Hold, "must hold during cooldown");
                }
            }
        }
    }
}
