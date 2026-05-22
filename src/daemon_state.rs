//! Reactive daemon lifecycle state machine.
//!
//! Combines an atomic state register (lock-free reads from any thread)
//! with reactive event emission (subscribers react to phase transitions).
//!
//! State register: AtomicU8 — always reflects the current phase, readable
//! even if the daemon is defunct or the channel is disconnected.
//!
//! Event emitter: Subject<DaemonPhase> — broadcasts transitions to all
//! subscribers. Components can use crossbeam::select! to multiplex
//! lifecycle events with their own work channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix epoch milliseconds via the system clock. `0` on the (impossible
/// in production) case where `SystemTime::now()` is before the epoch.
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use crossbeam_channel::Receiver;

use crate::reactive::subject::Subject;

/// Ordered lifecycle phases.
/// repr(u8) for AtomicU8 storage.
/// PartialOrd enables `is_at_least()` comparisons.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DaemonPhase {
    /// DB, model, pools created; not yet scanning files
    Initializing = 0,
    /// Initial file scan + embedding in progress
    Scanning = 1,
    /// Initial scan complete; all systems nominal
    Ready = 2,
    /// Orderly shutdown in progress
    Terminating = 3,
    /// Unrecoverable error; daemon is defunct
    Defunct = 4,
}

impl DaemonPhase {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Initializing,
            1 => Self::Scanning,
            2 => Self::Ready,
            3 => Self::Terminating,
            _ => Self::Defunct,
        }
    }

    /// Human-readable label for logging.
    pub fn label(self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::Scanning => "scanning",
            Self::Ready => "ready",
            Self::Terminating => "terminating",
            Self::Defunct => "defunct",
        }
    }
}

impl std::fmt::Display for DaemonPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Shared handle for the daemon lifecycle.
///
/// Clone-cheap (all fields are Arc). Pass to any component that needs
/// to read, transition, or subscribe to lifecycle state.
#[derive(Clone)]
pub struct DaemonLifecycle {
    /// Atomic state register — always reflects current phase.
    phase: Arc<AtomicU8>,
    /// Unix epoch millis when the current phase was first entered.
    /// Used by heavy-cron `Cooldown` logs so an operator can see
    /// "in Ready for X seconds, waiting for Y" without consulting
    /// process start time externally. `0` until the first transition.
    phase_started_at_ms: Arc<AtomicI64>,
    /// Reactive event channel — broadcasts phase transitions.
    subject: Arc<Subject<DaemonPhase>>,
}

impl Default for DaemonLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonLifecycle {
    pub fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(DaemonPhase::Initializing as u8)),
            phase_started_at_ms: Arc::new(AtomicI64::new(now_unix_ms())),
            subject: Arc::new(Subject::new(16)),
        }
    }

    /// Atomically transition to a new phase and broadcast the event.
    ///
    /// Uses `fetch_max` to enforce monotonic forward transitions —
    /// a component cannot move the daemon backwards (e.g. Ready → Scanning).
    /// Exception: `Defunct` (4) can be set from any state.
    ///
    /// Returns the previous phase.
    pub fn transition(&self, to: DaemonPhase) -> DaemonPhase {
        let prev = self.phase.fetch_max(to as u8, Ordering::AcqRel);
        let prev_phase = DaemonPhase::from_u8(prev);
        if prev_phase != to && prev < to as u8 {
            self.phase_started_at_ms
                .store(now_unix_ms(), Ordering::Release);
            tracing::info!(
                from = %prev_phase,
                to = %to,
                "Daemon phase transition"
            );
            self.subject.next(to);
        }
        prev_phase
    }

    /// Unix epoch milliseconds when the current phase was first entered.
    /// `0` if no transition has happened yet (unlikely — the
    /// constructor records the initialization moment).
    pub fn phase_started_at_ms(&self) -> i64 {
        self.phase_started_at_ms.load(Ordering::Acquire)
    }

    /// Milliseconds since the daemon entered its current phase. Useful
    /// for "in Ready for X ms, waiting for Y" diagnostic logs in
    /// heavy-cron cooldown skips.
    pub fn ms_in_current_phase(&self) -> i64 {
        let started = self.phase_started_at_ms();
        if started == 0 {
            return 0;
        }
        (now_unix_ms() - started).max(0)
    }

    /// Current phase (lock-free atomic read).
    /// Safe to call from any thread, any time — even if daemon is defunct.
    pub fn current(&self) -> DaemonPhase {
        DaemonPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Check if daemon has reached at least the given phase.
    pub fn is_at_least(&self, phase: DaemonPhase) -> bool {
        self.current() >= phase
    }

    /// True if the daemon is in a healthy running state (Ready).
    #[allow(dead_code)] // Used by tests and future health-check endpoints
    pub fn is_healthy(&self) -> bool {
        self.current() == DaemonPhase::Ready
    }

    /// True if the daemon is shutting down or defunct.
    pub fn is_stopping(&self) -> bool {
        self.current() >= DaemonPhase::Terminating
    }

    /// Subscribe to phase transition events.
    ///
    /// Returns a crossbeam Receiver. Use in `crossbeam::select!` to
    /// multiplex with work channels, or iterate to react to each transition.
    #[allow(dead_code)] // Used by tests and future reactive components
    pub fn subscribe(&self) -> Receiver<DaemonPhase> {
        self.subject.receiver()
    }

    /// Block until the daemon reaches at least the given phase.
    ///
    /// Returns `true` if the target phase was reached.
    /// Returns `false` if the daemon became defunct or the channel disconnected
    /// before reaching the target.
    #[allow(dead_code)] // Used by tests and future blocking waiters
    pub fn wait_for(&self, target: DaemonPhase) -> bool {
        let current = self.current();
        // Already at or past the target — but Terminating/Defunct mean failure
        // unless the target itself is Terminating/Defunct.
        if current >= target {
            // If we reached the exact target or a healthy superset, succeed.
            // If we overshot into Terminating/Defunct while waiting for a
            // healthy phase (Scanning/Ready), that's a failure.
            return current == target
                || target >= DaemonPhase::Terminating
                || current < DaemonPhase::Terminating;
        }
        let rx = self.subscribe();
        for phase in rx {
            if phase == target {
                return true;
            }
            // If we reach a healthy phase past the target, succeed
            if phase > target && phase < DaemonPhase::Terminating {
                return true;
            }
            // If we hit Terminating/Defunct while waiting for a healthy phase, fail
            if phase >= DaemonPhase::Terminating && target < DaemonPhase::Terminating {
                return false;
            }
        }
        // Channel disconnected — final check
        let final_phase = self.current();
        final_phase >= target
            && (target >= DaemonPhase::Terminating || final_phase < DaemonPhase::Terminating)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_phase() {
        let lc = DaemonLifecycle::new();
        assert_eq!(lc.current(), DaemonPhase::Initializing);
        assert!(!lc.is_at_least(DaemonPhase::Ready));
        assert!(!lc.is_healthy());
        assert!(!lc.is_stopping());
    }

    #[test]
    fn test_forward_transitions() {
        let lc = DaemonLifecycle::new();
        let prev = lc.transition(DaemonPhase::Scanning);
        assert_eq!(prev, DaemonPhase::Initializing);
        assert_eq!(lc.current(), DaemonPhase::Scanning);

        lc.transition(DaemonPhase::Ready);
        assert_eq!(lc.current(), DaemonPhase::Ready);
        assert!(lc.is_at_least(DaemonPhase::Ready));
        assert!(lc.is_healthy());
    }

    #[test]
    fn test_backward_transition_rejected() {
        let lc = DaemonLifecycle::new();
        lc.transition(DaemonPhase::Ready);
        let prev = lc.transition(DaemonPhase::Scanning); // backward — should be rejected
        assert_eq!(prev, DaemonPhase::Ready); // fetch_max kept Ready
        assert_eq!(lc.current(), DaemonPhase::Ready); // still Ready
    }

    #[test]
    fn test_defunct_from_any_state() {
        let lc = DaemonLifecycle::new();
        lc.transition(DaemonPhase::Ready);
        lc.transition(DaemonPhase::Defunct);
        assert_eq!(lc.current(), DaemonPhase::Defunct);
        assert!(lc.is_stopping());
    }

    #[test]
    fn test_subscriber_receives_transitions() {
        let lc = DaemonLifecycle::new();
        let rx = lc.subscribe();
        lc.transition(DaemonPhase::Scanning);
        lc.transition(DaemonPhase::Ready);
        assert_eq!(
            rx.recv().expect("should receive Scanning"),
            DaemonPhase::Scanning
        );
        assert_eq!(rx.recv().expect("should receive Ready"), DaemonPhase::Ready);
    }

    #[test]
    fn test_wait_for_already_reached() {
        let lc = DaemonLifecycle::new();
        lc.transition(DaemonPhase::Ready);
        assert!(lc.wait_for(DaemonPhase::Ready)); // immediate
    }

    #[test]
    fn test_wait_for_with_thread() {
        let lc = DaemonLifecycle::new();
        let lc_clone = lc.clone();
        let handle = std::thread::spawn(move || lc_clone.wait_for(DaemonPhase::Ready));
        // Small delay then transition
        std::thread::sleep(std::time::Duration::from_millis(50));
        lc.transition(DaemonPhase::Scanning);
        lc.transition(DaemonPhase::Ready);
        assert!(handle.join().expect("thread should not panic"));
    }

    #[test]
    fn test_wait_for_defunct_returns_false() {
        let lc = DaemonLifecycle::new();
        let lc_clone = lc.clone();
        let handle = std::thread::spawn(move || lc_clone.wait_for(DaemonPhase::Ready));
        std::thread::sleep(std::time::Duration::from_millis(50));
        lc.transition(DaemonPhase::Defunct);
        assert!(!handle.join().expect("thread should not panic"));
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    use proptest::prelude::*;

    fn phase_strategy() -> impl Strategy<Value = DaemonPhase> {
        prop_oneof![
            Just(DaemonPhase::Initializing),
            Just(DaemonPhase::Scanning),
            Just(DaemonPhase::Ready),
            Just(DaemonPhase::Terminating),
            Just(DaemonPhase::Defunct),
        ]
    }

    proptest! {
        /// For any sequence of transitions, the final phase equals the
        /// max(seq). Proves the fetch_max-based transition semantics.
        #[test]
        fn prop_final_phase_is_max_of_requested(
            transitions in prop::collection::vec(phase_strategy(), 1..10usize),
        ) {
            let lc = DaemonLifecycle::new();
            let mut max_seen = DaemonPhase::Initializing;
            for p in &transitions {
                lc.transition(*p);
                if *p > max_seen {
                    max_seen = *p;
                }
            }
            prop_assert_eq!(lc.current(), max_seen);
        }

        /// Backward transitions are ignored (monotone non-decreasing).
        #[test]
        fn prop_backward_transitions_ignored(
            forward in phase_strategy(),
            backward in phase_strategy(),
        ) {
            prop_assume!(backward < forward);
            let lc = DaemonLifecycle::new();
            lc.transition(forward);
            let prev = lc.current();
            lc.transition(backward);
            prop_assert_eq!(lc.current(), prev,
                "backward transition changed state: {:?} → {:?}", prev, lc.current());
        }

        /// Every subscriber receives every transition — no messages lost.
        /// Uses indexed picks so the monotone chain is chosen, not
        /// filtered, avoiding global-reject blowups.
        #[test]
        fn prop_subject_broadcasts_monotone_transitions(
            i in 1usize..=3,  // start index in the monotone chain
            j in 2usize..=4,  // middle index
            k in 3usize..=4,  // end index
        ) {
            let chain = [
                DaemonPhase::Initializing,
                DaemonPhase::Scanning,
                DaemonPhase::Ready,
                DaemonPhase::Terminating,
                DaemonPhase::Defunct,
            ];
            // Enforce ordering i < j < k
            let (start, middle, end) = if i < j && j < k {
                (chain[i], chain[j], chain[k])
            } else {
                return Ok(());
            };
            let lc = DaemonLifecycle::new();
            let rx = lc.subscribe();
            lc.transition(start);
            lc.transition(middle);
            lc.transition(end);
            prop_assert_eq!(rx.recv().expect("start"), start);
            prop_assert_eq!(rx.recv().expect("middle"), middle);
            prop_assert_eq!(rx.recv().expect("end"), end);
        }
    }
}
