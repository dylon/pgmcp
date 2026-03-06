//! Subject - both Observable and Observer.
//!
//! Emits values to all subscribers via a crossbeam channel.

use crossbeam_channel::{bounded, Sender, Receiver};

use super::observable::Observable;

/// Subject that can emit values and be subscribed to.
///
/// Uses a bounded crossbeam channel internally.
pub struct Subject<T: Send + 'static> {
    tx: Sender<T>,
    rx: Receiver<T>,
}

impl<T: Send + Clone + 'static> Subject<T> {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        Self { tx, rx }
    }

    /// Emit a value to subscribers.
    pub fn next(&self, value: T) {
        let _ = self.tx.send(value);
    }

    /// Get an Observable view of this subject.
    pub fn as_observable(&self) -> Observable<T> {
        Observable::from_receiver(self.rx.clone())
    }

    /// Get a clone of the sender (for multi-producer use).
    pub fn sender(&self) -> Sender<T> {
        self.tx.clone()
    }

    /// Get a clone of the receiver.
    pub fn receiver(&self) -> Receiver<T> {
        self.rx.clone()
    }
}
