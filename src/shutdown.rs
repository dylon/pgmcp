use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
