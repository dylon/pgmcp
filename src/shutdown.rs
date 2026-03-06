use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
