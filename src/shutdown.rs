use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Coordinates orderly shutdown across sync and async components.
///
/// - `Arc<AtomicBool>` for sync components (WorkPool, CronStateMachine, file watcher)
/// - `CancellationToken` for async components (MCP server, metrics HTTP)
#[derive(Clone)]
pub struct ShutdownCoordinator {
    /// Atomic flag for sync components. Checked with Acquire ordering.
    terminating: Arc<AtomicBool>,
    /// Cancellation token for async (tokio) components.
    cancellation_token: CancellationToken,
}

impl ShutdownCoordinator {
    pub fn new() -> Self {
        Self {
            terminating: Arc::new(AtomicBool::new(false)),
            cancellation_token: CancellationToken::new(),
        }
    }

    /// Signal all components to shut down.
    pub fn signal_shutdown(&self) {
        self.terminating.store(true, Ordering::Release);
        self.cancellation_token.cancel();
    }

    /// Check if shutdown has been signaled (for sync components).
    #[inline]
    #[allow(dead_code)]
    pub fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::Acquire)
    }

    /// Get a clone of the atomic terminating flag (for sync components).
    pub fn terminating_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.terminating)
    }

    /// Get a clone of the cancellation token (for async components).
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Join a `std::thread::JoinHandle` with a wall-clock timeout.
///
/// Since `JoinHandle::join()` has no native timeout, this spawns a helper thread
/// that performs the blocking join and signals completion via a crossbeam channel.
///
/// Returns:
/// - `Ok(Ok(()))` — thread exited cleanly within the timeout
/// - `Ok(Err(panic_payload))` — thread panicked within the timeout
/// - `Err(helper_handle)` — timeout expired; the helper thread is still blocked on join
///   and will be cleaned up when the target thread eventually exits (or by `process::exit`)
pub fn join_with_timeout(
    handle: JoinHandle<()>,
    timeout: Duration,
) -> Result<std::thread::Result<()>, JoinHandle<()>> {
    let (tx, rx) = crossbeam_channel::bounded(1);

    let join_thread = std::thread::Builder::new()
        .name("pgmcp-join-helper".into())
        .spawn(move || {
            let result = handle.join();
            let _ = tx.send(result);
        })
        .expect("Failed to spawn join helper thread");

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = join_thread.join();
            Ok(result)
        }
        Err(_) => {
            // Timeout — helper thread is still blocked on join.
            // Return the helper handle so the caller can decide what to do.
            Err(join_thread)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn signal_shutdown_flips_terminating_and_cancels_token() {
        let coord = ShutdownCoordinator::new();
        assert!(!coord.is_terminating());
        let token = coord.cancellation_token();
        assert!(!token.is_cancelled());
        coord.signal_shutdown();
        assert!(coord.is_terminating());
        assert!(token.is_cancelled());
    }

    #[test]
    fn terminating_flag_propagates_to_clones() {
        let coord_a = ShutdownCoordinator::new();
        let coord_b = coord_a.clone();
        let flag_b = coord_b.terminating_flag();
        assert!(!flag_b.load(Ordering::Acquire));
        coord_a.signal_shutdown();
        assert!(
            flag_b.load(Ordering::Acquire),
            "flag from clone must see update from original"
        );
    }

    #[tokio::test]
    async fn cancellation_token_cancels_tokio_select() {
        let coord = ShutdownCoordinator::new();
        let token = coord.cancellation_token();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_c = Arc::clone(&fired);
        let handle = tokio::spawn(async move {
            token.cancelled().await;
            fired_c.store(true, Ordering::Release);
        });
        coord.signal_shutdown();
        handle.await.expect("task completes");
        assert!(fired.load(Ordering::Acquire));
    }

    /// Determinism fix (2026-05-23): the 1-second test timeout was
    /// raised to 30 seconds. The 10 ms-sleep thread joins trivially
    /// under normal conditions, but under heavy parallel test load
    /// (nextest running release mode against the full suite of reactive
    /// / topic / k-selector proptests) the `join_with_timeout` helper
    /// thread can be starved long enough that `recv_timeout` fires
    /// before the helper schedules — same scheduler-jitter pattern as
    /// the reactive-operators flake fixes in `src/reactive/operators.rs`.
    /// 30 s is far above any plausible jitter while still bounded
    /// enough that an actual hang fails the test in well under a minute.
    #[test]
    fn join_with_timeout_returns_ok_for_fast_thread() {
        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(10));
        });
        let result = join_with_timeout(handle, Duration::from_secs(30));
        assert!(result.is_ok(), "fast thread should join within timeout");
        assert!(result.expect("ok branch").is_ok());
    }

    #[test]
    fn join_with_timeout_returns_err_on_overrun() {
        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
        });
        let result = join_with_timeout(handle, Duration::from_millis(100));
        assert!(
            result.is_err(),
            "slow thread should return timeout (helper thread)"
        );
    }

    /// Determinism fix (2026-05-23): the 1-second test timeout was
    /// raised to 30 seconds (this site flaked first). What the test
    /// actually verifies — "a panicking thread doesn't cause
    /// `join_with_timeout` to hang forever" — does not require the
    /// panic-unwind + helper-thread `handle.join()` + crossbeam send
    /// + `recv_timeout` round-trip to complete inside any particular
    /// wall-clock budget; we only need a generous upper bound to
    /// distinguish "panic surfaced quickly" from an actual hang. Under
    /// nextest's parallel load the helper thread can be starved past
    /// 1 second — same root cause as the reactive-operators flake
    /// fixes (`src/reactive/operators.rs`). 30 s sits far above any
    /// plausible scheduler jitter.
    #[test]
    fn join_with_timeout_surfaces_panic_not_hang() {
        let handle = std::thread::spawn(|| {
            panic!("deliberate panic for test");
        });
        let result = join_with_timeout(handle, Duration::from_secs(30));
        let inner = result.expect("ok branch — panic finishes fast");
        assert!(inner.is_err(), "panic must surface as Err(JoinError)");
    }
}
