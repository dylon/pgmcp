//! Phase 4 regression tests for `pgmcp::fuzzy::FuzzyIndex`.
//!
//! Covers daemon-restart durability: insert N terms into a fresh trie,
//! drop the index, re-open, confirm every term recovers with the
//! correct value payload. Backstops the disk-backed
//! `PersistentARTrieChar` integration used by every Phase-8 fuzzy MCP
//! tool.

use pgmcp::fuzzy::{FuzzyIndex, SymbolValue};
use tempfile::tempdir;

#[test]
fn n_term_round_trip_survives_drop_and_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("symbols.artrie");

    // First open: populate.
    let (idx, recovery) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("create");
    assert!(recovery.is_none(), "fresh trie has no recovery work");
    for i in 0..256i64 {
        idx.upsert(
            &format!("symbol_{:04}", i),
            SymbolValue {
                file_id: i,
                kind: "function".into(),
                visibility: "public".into(),
                line: i as i32,
            },
        )
        .expect("upsert");
    }
    assert_eq!(idx.len(), 256);
    drop(idx);

    // Re-open: every term + value must come back.
    let (idx2, recovery2) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("reopen");
    assert!(
        recovery2.is_some(),
        "re-open should produce a recovery report"
    );
    assert_eq!(idx2.len(), 256);
    for i in 0..256i64 {
        let key = format!("symbol_{:04}", i);
        let v = idx2.get(&key).expect("term recovered");
        assert_eq!(v.file_id, i);
        assert_eq!(v.kind, "function");
        assert_eq!(v.visibility, "public");
        assert_eq!(v.line, i as i32);
    }
}

#[test]
fn fuzzy_query_after_reopen_returns_hits() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("query.artrie");

    let (idx, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("create");
    for term in ["receive", "decide", "recipe", "rebuild"] {
        idx.upsert(
            term,
            SymbolValue {
                file_id: 1,
                kind: "function".into(),
                visibility: "public".into(),
                line: 1,
            },
        )
        .unwrap();
    }
    drop(idx);

    let (idx2, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("reopen");
    let hits = idx2.query("recieve", 2);
    assert!(
        hits.iter().any(|(t, _, _)| t == "receive"),
        "post-recovery fuzzy query must find `receive`: got {:?}",
        hits.iter()
            .map(|(t, d, _)| (t.clone(), *d))
            .collect::<Vec<_>>()
    );
}

#[test]
fn remove_persists_across_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("remove.artrie");

    let (idx, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("create");
    idx.upsert(
        "doomed",
        SymbolValue {
            file_id: 99,
            kind: "trait".into(),
            visibility: "public".into(),
            line: 1,
        },
    )
    .unwrap();
    assert!(idx.contains("doomed"));
    let removed = idx.remove("doomed").unwrap();
    assert!(removed);
    assert!(!idx.contains("doomed"));
    drop(idx);

    let (idx2, _) = FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("reopen");
    assert!(
        !idx2.contains("doomed"),
        "remove must persist across reopen"
    );
}
