//! Observable - push-based stream of values using crossbeam channels.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};

use super::subscription::Subscription;

/// Push-based stream of values backed by a crossbeam channel.
pub struct Observable<T: Send + 'static> {
    rx: Receiver<T>,
}

impl<T: Send + 'static> Observable<T> {
    /// Create an observable from a crossbeam receiver.
    pub fn from_receiver(rx: Receiver<T>) -> Self {
        Self { rx }
    }

    /// Create an observable and its sender.
    pub fn channel(capacity: usize) -> (Sender<T>, Self) {
        let (tx, rx) = bounded(capacity);
        (tx, Self { rx })
    }

    /// Get the underlying receiver (for composition).
    pub fn into_receiver(self) -> Receiver<T> {
        self.rx
    }

    /// Filter values by a predicate.
    pub fn filter(self, pred: impl Fn(&T) -> bool + Send + 'static) -> Observable<T> {
        let (tx, new_obs) = Observable::channel(256);
        let rx = self.rx;

        thread::Builder::new()
            .name("rx-filter".into())
            .spawn(move || {
                for item in rx {
                    if pred(&item) && tx.send(item).is_err() {
                        break;
                    }
                }
            })
            .expect("Failed to spawn filter thread");

        new_obs
    }

    /// Map values through a function.
    pub fn map<U: Send + 'static>(self, f: impl Fn(T) -> U + Send + 'static) -> Observable<U> {
        let (tx, new_obs) = Observable::channel(256);
        let rx = self.rx;

        thread::Builder::new()
            .name("rx-map".into())
            .spawn(move || {
                for item in rx {
                    if tx.send(f(item)).is_err() {
                        break;
                    }
                }
            })
            .expect("Failed to spawn map thread");

        new_obs
    }

    /// Merge two observables into one.
    pub fn merge(self, other: Observable<T>) -> Observable<T> {
        let (tx, new_obs) = Observable::channel(256);
        let rx1 = self.rx;
        let rx2 = other.rx;

        thread::Builder::new()
            .name("rx-merge".into())
            .spawn(move || {
                loop {
                    crossbeam_channel::select! {
                        recv(rx1) -> msg => {
                            match msg {
                                Ok(item) => { if tx.send(item).is_err() { break; } }
                                Err(_) => break,
                            }
                        }
                        recv(rx2) -> msg => {
                            match msg {
                                Ok(item) => { if tx.send(item).is_err() { break; } }
                                Err(_) => break,
                            }
                        }
                    }
                }
            })
            .expect("Failed to spawn merge thread");

        new_obs
    }

    /// Subscribe to the observable with a callback.
    pub fn subscribe(self, on_next: impl Fn(T) + Send + 'static) -> Subscription {
        let (sub, cancelled) = Subscription::new();
        let rx = self.rx;

        thread::Builder::new()
            .name("rx-subscribe".into())
            .spawn(move || {
                for item in rx {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    on_next(item);
                }
            })
            .expect("Failed to spawn subscribe thread");

        sub
    }
}

/// Debounce values by key: for each key, only emit after `duration` of silence.
///
/// Uses the same drain-and-reinsert pattern as `operators::debounce_by_key`.
pub fn debounce_by_key_observable<T, K>(
    rx: Receiver<T>,
    duration: Duration,
    key_fn: impl Fn(&T) -> K + Send + 'static,
) -> Observable<T>
where
    T: Send + 'static,
    K: Eq + std::hash::Hash + Send + 'static,
{
    let inner_rx = super::operators::debounce_by_key(rx, duration, key_fn);
    Observable::from_receiver(inner_rx)
}
