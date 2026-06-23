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

/// Dedup with a time-to-live: emit an item only if its key has **not** been
/// emitted within the last `ttl`; otherwise suppress it (a keyed throttle-first).
///
/// This is the burst-collapser for the file-event ingestion stream (ADR-022): a
/// `cargo build` re-`open`s the same headers thousands of times, and we want at
/// most one row per `(actor, op, path)` per window. It is the standalone,
/// reusable form of the hand-rolled dedup that previously lived inside
/// `proc_clients::ebpf::handle_event` (same 4096-entry opportunistic eviction).
pub fn dedup_ttl<T, K, F>(rx: Receiver<T>, ttl: Duration, key_fn: F) -> Receiver<T>
where
    T: Send + 'static,
    K: Eq + Hash + Send + 'static,
    F: Fn(&T) -> K + Send + 'static,
{
    let (tx, out_rx) = bounded(256);

    thread::Builder::new()
        .name("rx-dedup-ttl".into())
        .spawn(move || {
            let mut seen: HashMap<K, Instant> = HashMap::new();
            for item in rx {
                let now = Instant::now();
                let k = key_fn(&item);
                if let Some(prev) = seen.get(&k)
                    && now.duration_since(*prev) < ttl
                {
                    continue; // same key within the window — collapse
                }
                seen.insert(k, now);
                // Opportunistic bound: when the map grows large, drop entries
                // whose window has already elapsed (cannot suppress anything).
                if seen.len() > 4096 {
                    seen.retain(|_, t| now.duration_since(*t) < ttl);
                }
                if tx.send(item).is_err() {
                    break;
                }
            }
        })
        .expect("Failed to spawn dedup_ttl thread");

    out_rx
}

/// Buffer items into a `Vec`, flushing when **either** `max_count` items have
/// accumulated **or** `window` elapses with a non-empty buffer — whichever comes
/// first. This bounds both latency (the window) and batch size (the count) so a
/// flood within one window cannot grow an unbounded batch. Used by the file-event
/// writer to coalesce many touches into one multi-row INSERT.
pub fn buffer_time_count<T: Send + 'static>(
    rx: Receiver<T>,
    window: Duration,
    max_count: usize,
) -> Receiver<Vec<T>> {
    let (tx, out_rx) = bounded(64);
    let cap = max_count.max(1);

    thread::Builder::new()
        .name("rx-buffer-time-count".into())
        .spawn(move || {
            let mut buf: Vec<T> = Vec::with_capacity(cap);
            let mut deadline = Instant::now() + window;
            loop {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(timeout) {
                    Ok(item) => {
                        buf.push(item);
                        if buf.len() >= cap {
                            let batch = std::mem::replace(&mut buf, Vec::with_capacity(cap));
                            if tx.send(batch).is_err() {
                                break;
                            }
                            deadline = Instant::now() + window;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if !buf.is_empty() {
                            let batch = std::mem::replace(&mut buf, Vec::with_capacity(cap));
                            if tx.send(batch).is_err() {
                                break;
                            }
                        }
                        deadline = Instant::now() + window;
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
        .expect("Failed to spawn buffer_time_count thread");

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
        // Window must be >> the negative-recv timeout so 2 and 3 stay
        // suppressed even when the worker thread is descheduled under
        // heavy parallel test load (verify.sh runs all 763 tests at once).
        // Original (100ms window vs 50ms wait) was a fragile 2:1 ratio;
        // 2s vs 100ms gives a 20:1 safety margin.
        let throttle_window = Duration::from_millis(2_000);
        let (tx, rx) = unbounded();
        let throttled = throttle_first(rx, throttle_window);

        tx.send(1).expect("send failed");
        tx.send(2).expect("send failed");
        tx.send(3).expect("send failed");

        // Generous positive timeout: the first emission must arrive,
        // but the worker thread can be stalled tens of ms under load.
        // Only consumed in full if throttle_first is broken.
        let first = throttled
            .recv_timeout(Duration::from_secs(2))
            .expect("recv failed");
        assert_eq!(first, 1);

        // 2 and 3 must be suppressed. 100ms is comfortably inside the
        // 2s window even with scheduler jitter.
        assert!(
            throttled.recv_timeout(Duration::from_millis(100)).is_err(),
            "throttle_first leaked a second emission inside the window"
        );

        drop(tx);
    }

    /// Stress-loop the fix for test_throttle_first to catch regressions
    /// where someone tightens the timing budget. Runs 50 iterations of
    /// the same scenario; any flake fails the build deterministically.
    #[test]
    fn test_throttle_first_is_deterministic_under_load() {
        let throttle_window = Duration::from_millis(2_000);
        for iter in 0..50 {
            let (tx, rx) = unbounded();
            let throttled = throttle_first(rx, throttle_window);

            tx.send(1).expect("send failed");
            tx.send(2).expect("send failed");
            tx.send(3).expect("send failed");

            let first = throttled
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("iter {iter}: positive recv failed: {e:?}"));
            assert_eq!(first, 1, "iter {iter}: first emission must be 1");

            assert!(
                throttled.recv_timeout(Duration::from_millis(100)).is_err(),
                "iter {iter}: throttle leaked second emission inside window"
            );

            drop(tx);
        }
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

        // Drain via blocking recv() — the worker thread exits after its
        // input disconnects, deterministically closing this channel.
        // Using recv_timeout here would risk a false negative under load.
        let mut results = Vec::new();
        while let Ok(v) = distinct.recv() {
            results.push(v);
        }
        assert_eq!(results, vec![1, 2, 3]);
    }

    #[test]
    fn test_dedup_ttl_suppresses_within_window() {
        let (tx, rx) = unbounded();
        // A long TTL so every repeat inside this test is suppressed.
        let deduped = dedup_ttl(rx, Duration::from_secs(3600), |k: &i32| *k);
        for v in [1, 1, 2, 1, 2] {
            tx.send(v).expect("send failed");
        }
        drop(tx);
        let mut out = Vec::new();
        while let Ok(v) = deduped.recv() {
            out.push(v);
        }
        // First 1 and first 2 pass; subsequent repeats are within-window dups.
        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn test_buffer_time_count_flushes_on_count() {
        let (tx, rx) = unbounded();
        // A long window so only the count threshold triggers the in-band flushes.
        let batched = buffer_time_count(rx, Duration::from_secs(3600), 3);
        for i in 0..7 {
            tx.send(i).expect("send failed");
        }
        drop(tx);
        let mut batches = Vec::new();
        while let Ok(b) = batched.recv() {
            batches.push(b);
        }
        // 7 items, cap 3 → two full batches then the disconnect-flush remainder.
        assert_eq!(batches, vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]);
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
            for batch in buffered {
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

            let batches: Vec<Vec<i32>> = buffered.iter().collect();

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

            let results: Vec<i32> = distinct.iter().collect();

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

            let results: Vec<i32> = throttled.iter().collect();

            // Output count must be <= input count
            prop_assert!(results.len() <= items.len());
            // Must emit at least the first item
            prop_assert!(!results.is_empty());
            // First output must be first input
            prop_assert_eq!(results[0], items[0]);
        }
    }

    // ========================================================================
    // Proptests: buffer_time + debounce_by_key
    // ========================================================================

    proptest! {
        /// buffer_time flushes items it has accumulated when the window
        /// elapses. Every item sent before the deadline is present in the
        /// concatenated output.
        ///
        /// Determinism fix (2026-05-22): the consumer uses blocking
        /// `recv()` rather than `recv_timeout()`. The semantics this test
        /// is verifying — "every item sent before tx-drop reaches the
        /// output" — do not actually depend on wall-clock timing once tx
        /// is closed: the buffer_time worker thread sees Disconnected,
        /// flushes its remaining buffer, and exits, which closes the
        /// output channel. `recv()` then returns Err on close, exiting
        /// the loop. The previous `recv_timeout(500ms)` produced a flaky
        /// "got [] expected [...]" failure under heavy parallel proptest
        /// load when the worker thread didn't get scheduled inside the
        /// timeout — a scheduler artifact, not a real semantics bug.
        #[test]
        fn prop_buffer_time_delivers_all_items(
            items in prop::collection::vec(any::<i32>(), 1..30usize),
        ) {
            let (tx, rx) = unbounded();
            let buffered = buffer_time(rx, Duration::from_millis(20));
            for &item in &items {
                tx.send(item).expect("send");
            }
            drop(tx);
            // Block on the output channel until the worker thread closes
            // it (after flushing on Disconnected). No wall-clock timeout
            // — the worker is guaranteed to terminate once `tx` is gone.
            let mut out: Vec<i32> = Vec::new();
            while let Ok(batch) = buffered.recv() {
                out.extend(batch);
            }
            prop_assert_eq!(out.len(), items.len(),
                "buffer_time must preserve all items: got {:?}, expected {:?}", out, items);
        }

        /// debounce_by_key coalesces repeated sends on the same key within
        /// a window into one emission. For distinct keys, every key is
        /// represented at least once in the output.
        ///
        /// Determinism fix (2026-05-22): same fix as
        /// `prop_buffer_time_delivers_all_items` — use blocking `recv()`
        /// on the output channel. The debounce worker thread terminates
        /// after its input channel disconnects (drains pending into the
        /// output, then breaks). The output channel then closes
        /// deterministically, returning Err to `recv()`. The previous
        /// `recv_timeout(500ms)` produced false negatives under heavy
        /// parallel proptest load when the worker thread didn't get
        /// scheduled inside the wall-clock window.
        #[test]
        fn prop_debounce_by_key_emits_at_least_one_per_key(
            keys in prop::collection::vec(0u32..5, 1..30usize),
        ) {
            let (tx, rx) = unbounded();
            let debounced = debounce_by_key(rx, Duration::from_millis(50), |k: &u32| *k);
            for &k in &keys {
                tx.send(k).expect("send");
            }
            drop(tx);
            let mut out = Vec::new();
            while let Ok(v) = debounced.recv() {
                out.push(v);
            }
            let in_keys: std::collections::HashSet<u32> = keys.iter().copied().collect();
            let out_keys: std::collections::HashSet<u32> = out.iter().copied().collect();
            for k in &in_keys {
                prop_assert!(out_keys.contains(k),
                    "key {} never emitted: input={:?}, output={:?}", k, keys, out);
            }
            // Output count ≤ input count (coalescing never invents items).
            prop_assert!(out.len() <= keys.len());
        }

        /// throttle_first emits exactly one item per throttle window.
        /// When the total send window < throttle_interval, exactly one
        /// item lands.
        #[test]
        fn prop_throttle_first_emits_one_per_window(
            items in prop::collection::vec(any::<i32>(), 2..10usize),
        ) {
            let (tx, rx) = unbounded();
            let throttled = throttle_first(rx, Duration::from_millis(500));
            for &item in &items {
                tx.send(item).expect("send");
            }
            drop(tx);
            let out: Vec<i32> = throttled.iter().collect();
            // With a 500ms window and instant sends, exactly 1 item emits
            // before the window closes (the first one).
            prop_assert_eq!(out.len(), 1);
            prop_assert_eq!(out[0], items[0]);
        }
    }
}
