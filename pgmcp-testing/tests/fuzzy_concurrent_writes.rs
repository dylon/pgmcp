//! Phase 4 concurrency stress for `pgmcp::fuzzy::FuzzyIndex`.
//!
//! N-thread concurrent upsert + query, then exact-membership check on
//! every inserted term. The underlying `SharedCharARTrie`'s RwLock
//! semantics guarantee no lost updates or torn reads — this test
//! pins that contract.

use pgmcp::fuzzy::{FuzzyIndex, SymbolValue};
use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

#[test]
fn sixteen_thread_concurrent_upsert_loses_nothing() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("concurrent.artrie");
    let (idx, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("create");
    let idx = Arc::new(idx);

    let mut handles = Vec::with_capacity(16);
    for worker in 0..16 {
        let idx_for_worker = Arc::clone(&idx);
        handles.push(thread::spawn(move || {
            for i in 0..64 {
                let id = (worker as i64) * 1000 + i;
                let term = format!("worker_{worker}_term_{i}");
                idx_for_worker
                    .upsert(
                        &term,
                        SymbolValue {
                            file_id: id,
                            kind: "function".into(),
                            visibility: "public".into(),
                            line: i as i32,
                        },
                    )
                    .expect("upsert");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker join");
    }

    // 16 workers × 64 terms = 1024 unique entries.
    assert_eq!(idx.len(), 16 * 64);

    // Spot-check every inserted term.
    for worker in 0..16 {
        for i in 0..64 {
            let term = format!("worker_{worker}_term_{i}");
            let v = idx.get(&term).expect("present");
            assert_eq!(v.file_id, (worker as i64) * 1000 + i);
        }
    }
}

#[test]
fn concurrent_reads_during_writes_see_consistent_values() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("rw.artrie");
    let (idx, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("create");
    let idx = Arc::new(idx);

    // Pre-seed.
    for i in 0..32 {
        idx.upsert(
            &format!("seed_{i}"),
            SymbolValue {
                file_id: i,
                kind: "function".into(),
                visibility: "public".into(),
                line: i as i32,
            },
        )
        .unwrap();
    }

    // 4 writers, 4 readers in parallel; readers MUST never observe a
    // value with the wrong file_id for a known seed term.
    let mut handles = Vec::with_capacity(8);
    for worker in 0..4 {
        let idx_for_worker = Arc::clone(&idx);
        handles.push(thread::spawn(move || {
            for i in 0..256 {
                let term = format!("w{worker}_{i}");
                idx_for_worker
                    .upsert(
                        &term,
                        SymbolValue {
                            file_id: 1000 + i,
                            kind: "function".into(),
                            visibility: "public".into(),
                            line: i as i32,
                        },
                    )
                    .unwrap();
            }
        }));
    }
    for _ in 0..4 {
        let idx_for_reader = Arc::clone(&idx);
        handles.push(thread::spawn(move || {
            for _ in 0..256 {
                for i in 0..32 {
                    let term = format!("seed_{i}");
                    if let Some(v) = idx_for_reader.get(&term) {
                        assert_eq!(v.file_id, i, "seed term has unexpected file_id");
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker join");
    }
}
