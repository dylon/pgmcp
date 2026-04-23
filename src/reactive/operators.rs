//! Reactive operators for composing observable pipelines.
//!
//! These are implemented as standalone functions that transform
//! crossbeam Receivers into new Receivers.

use std::collections::HashMap;
use std::hash::Hash;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};

/// Buffer items until `count` is reached, then emit as a Vec.
pub fn buffer_count<T: Send + 'static>(rx: Receiver<T>, count: usize) -> Receiver<Vec<T>> {
    let (tx, out_rx) = bounded(64);

    thread::Builder::new()
        .name("rx-buffer-count".into())
        .spawn(move || {
            let mut buf = Vec::with_capacity(count);
            for item in rx {
                buf.push(item);
                if buf.len() >= count {
                    let batch = std::mem::replace(&mut buf, Vec::with_capacity(count));
                    if tx.send(batch).is_err() {
                        break;
                    }
                }
            }
            // Flush remaining
            if !buf.is_empty() {
                let _ = tx.send(buf);
            }
        })
        .expect("Failed to spawn buffer_count thread");

    out_rx
}

/// Buffer items for a duration, then emit as a Vec.
#[allow(dead_code)]
pub fn buffer_time<T: Send + 'static>(rx: Receiver<T>, duration: Duration) -> Receiver<Vec<T>> {
    let (tx, out_rx) = bounded(64);

    thread::Builder::new()
        .name("rx-buffer-time".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut deadline = Instant::now() + duration;

            loop {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(timeout) {
                    Ok(item) => {
                        buf.push(item);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if !buf.is_empty() {
                            let batch = std::mem::take(&mut buf);
                            if tx.send(batch).is_err() {
                                break;
                            }
                        }
                        deadline = Instant::now() + duration;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        if !buf.is_empty() {
                            let _ = tx.send(std::mem::take(&mut buf));
                        }
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn buffer_time thread");

    out_rx
}

/// Throttle: emit the first item, then suppress for `duration`.
pub fn throttle_first<T: Send + 'static>(rx: Receiver<T>, duration: Duration) -> Receiver<T> {
    let (tx, out_rx) = bounded(256);

    thread::Builder::new()
        .name("rx-throttle".into())
        .spawn(move || {
            let mut last_emit = Instant::now() - duration;
            for item in rx {
                let now = Instant::now();
                if now.duration_since(last_emit) >= duration {
                    last_emit = now;
                    if tx.send(item).is_err() {
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn throttle thread");

    out_rx
}

/// Distinct until changed: suppress consecutive duplicate values.
pub fn distinct_until_changed<T: Send + PartialEq + Clone + 'static>(
    rx: Receiver<T>,
) -> Receiver<T> {
    let (tx, out_rx) = bounded(256);

    thread::Builder::new()
        .name("rx-distinct".into())
        .spawn(move || {
            let mut prev: Option<T> = None;
            for item in rx {
                let should_emit = match &prev {
                    Some(p) => *p != item,
                    None => true,
                };
                if should_emit {
                    prev = Some(item.clone());
                    if tx.send(item).is_err() {
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn distinct_until_changed thread");

    out_rx
}

/// Debounce by key: for each unique key, only emit after `duration` of silence.
pub fn debounce_by_key<T, K, F>(rx: Receiver<T>, duration: Duration, key_fn: F) -> Receiver<T>
where
    T: Send + 'static,
    K: Eq + Hash + Send + 'static,
    F: Fn(&T) -> K + Send + 'static,
{
    let (tx, out_rx) = bounded(256);

    thread::Builder::new()
        .name("rx-debounce-key".into())
        .spawn(move || {
            let mut pending: HashMap<K, (Instant, T)> = HashMap::new();
            let check_interval = duration / 4;

            loop {
                match rx.recv_timeout(check_interval) {
                    Ok(item) => {
                        let k = key_fn(&item);
                        pending.insert(k, (Instant::now(), item));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // Emit all pending
                        for (_, (_, item)) in pending.drain() {
                            let _ = tx.send(item);
                        }
                        break;
                    }
                }

                // Emit items that have been quiet for `duration`.
                // Drain all entries, re-insert unexpired, emit expired.
                let now = Instant::now();
                let mut to_emit: Vec<T> = Vec::new();
                let all: Vec<(K, (Instant, T))> = pending.drain().collect();
                for (k, (instant, item)) in all {
                    if now.duration_since(instant) >= duration {
                        to_emit.push(item);
                    } else {
                        pending.insert(k, (instant, item));
                    }
                }

                for item in to_emit {
                    if tx.send(item).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("Failed to spawn debounce thread");

    out_rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use proptest::prelude::*;

    #[test]
    fn test_buffer_count() {
        let (tx, rx) = unbounded();
        let buffered = buffer_count(rx, 3);

        for i in 0..7 {
            tx.send(i).expect("send failed");
        }
        drop(tx);

        let batch1 = buffered.recv().expect("recv failed");
        assert_eq!(batch1, vec![0, 1, 2]);
        let batch2 = buffered.recv().expect("recv failed");
        assert_eq!(batch2, vec![3, 4, 5]);
        let batch3 = buffered.recv().expect("recv failed");
        assert_eq!(batch3, vec![6]);
    }

    #[test]
    fn test_throttle_first() {
        let (tx, rx) = unbounded();
        let throttled = throttle_first(rx, Duration::from_millis(100));

        tx.send(1).expect("send failed");
        tx.send(2).expect("send failed");
        tx.send(3).expect("send failed");

        let first = throttled
            .recv_timeout(Duration::from_millis(50))
            .expect("recv failed");
        assert_eq!(first, 1);

        // 2 and 3 should be suppressed
        assert!(throttled.recv_timeout(Duration::from_millis(50)).is_err());

        drop(tx);
    }

    #[test]
    fn test_distinct_until_changed() {
        let (tx, rx) = unbounded();
        let distinct = distinct_until_changed(rx);

        tx.send(1).expect("send failed");
        tx.send(1).expect("send failed");
        tx.send(2).expect("send failed");
        tx.send(2).expect("send failed");
        tx.send(3).expect("send failed");
        drop(tx);

        let mut results = Vec::new();
        while let Ok(v) = distinct.recv_timeout(Duration::from_millis(100)) {
            results.push(v);
        }
        assert_eq!(results, vec![1, 2, 3]);
    }

    // ========================================================================
    // Proptest: buffer_count
    // ========================================================================

    proptest! {
        #[test]
        fn prop_buffer_count_preserves_all_items(
            items in prop::collection::vec(any::<i32>(), 0..200),
            count in 1usize..20,
        ) {
            let (tx, rx) = unbounded();
            let buffered = buffer_count(rx, count);

            for &item in &items {
                tx.send(item).expect("send failed");
            }
            drop(tx);

            let mut all = Vec::new();
            while let Ok(batch) = buffered.recv_timeout(Duration::from_millis(200)) {
                all.extend(batch);
            }

            prop_assert_eq!(&all, &items, "buffer_count must preserve all items in order");
        }

        #[test]
        fn prop_buffer_count_batch_sizes(
            items in prop::collection::vec(any::<i32>(), 1..200),
            count in 1usize..20,
        ) {
            let (tx, rx) = unbounded();
            let buffered = buffer_count(rx, count);

            for &item in &items {
                tx.send(item).expect("send failed");
            }
            drop(tx);

            let mut batches = Vec::new();
            while let Ok(batch) = buffered.recv_timeout(Duration::from_millis(200)) {
                batches.push(batch);
            }

            // All batches except possibly the last must have exactly `count` items
            for (i, batch) in batches.iter().enumerate() {
                if i < batches.len() - 1 {
                    prop_assert_eq!(batch.len(), count, "non-final batch must be full");
                } else {
                    prop_assert!(batch.len() <= count, "final batch must not exceed count");
                    prop_assert!(!batch.is_empty(), "final batch must not be empty");
                }
            }
        }
    }

    // ========================================================================
    // Proptest: distinct_until_changed
    // ========================================================================

    proptest! {
        #[test]
        fn prop_distinct_removes_consecutive_duplicates(
            items in prop::collection::vec(0i32..10, 0..200),
        ) {
            let (tx, rx) = unbounded();
            let distinct = distinct_until_changed(rx);

            for &item in &items {
                tx.send(item).expect("send failed");
            }
            drop(tx);

            let mut results = Vec::new();
            while let Ok(v) = distinct.recv_timeout(Duration::from_millis(200)) {
                results.push(v);
            }

            // Property 1: output count <= input count
            prop_assert!(results.len() <= items.len());

            // Property 2: no consecutive duplicates in output
            for pair in results.windows(2) {
                prop_assert_ne!(pair[0], pair[1], "consecutive duplicates in output");
            }

            // Property 3: output is a subsequence of input
            let mut input_iter = items.iter();
            for &out_val in &results {
                let found = input_iter.by_ref().any(|&v| v == out_val);
                prop_assert!(found, "output value {} not found in remaining input", out_val);
            }
        }
    }

    // ========================================================================
    // Proptest: throttle_first
    // ========================================================================

    proptest! {
        #[test]
        fn prop_throttle_output_leq_input(
            items in prop::collection::vec(any::<i32>(), 1..100),
        ) {
            let (tx, rx) = unbounded();
            let throttled = throttle_first(rx, Duration::from_millis(10));

            for &item in &items {
                tx.send(item).expect("send failed");
            }
            drop(tx);

            let mut results = Vec::new();
            while let Ok(v) = throttled.recv_timeout(Duration::from_millis(200)) {
                results.push(v);
            }

            // Output count must be <= input count
            prop_assert!(results.len() <= items.len());
            // Must emit at least the first item
            prop_assert!(!results.is_empty());
            // First output must be first input
            prop_assert_eq!(results[0], items[0]);
        }
    }
}
