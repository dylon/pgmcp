//! Subscription handle for reactive streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Handle to cancel a subscription. Drop to unsubscribe.
/// Uses AtomicBool for lock-free cancellation.
pub struct Subscription {
    cancelled: Arc<AtomicBool>,
}

impl Subscription {
    pub fn new() -> (Self, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Self {
                cancelled: Arc::clone(&cancelled),
            },
            cancelled,
        )
    }

    /// Cancel the subscription.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Check if this subscription is still active.
    pub fn is_active(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}
