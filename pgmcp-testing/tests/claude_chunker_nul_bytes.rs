//! Regression test for C.9 (the second-named "open bug" in the OOM
//! ledger §7): UTF-8 NUL-byte handling for Claude JSONL.
//!
//! `serde_json::from_str` decodes JSON `\u0000` escapes to a literal
//! NUL byte (`\0`) in the resulting Rust `String`. Without intervention,
//! that NUL would flow into the chunker output and then into
//! `INSERT INTO file_chunks (content, ...)`, which Postgres rejects
//! with `invalid byte sequence for encoding "UTF8": 0x00`.
//!
//! The fix lives in two layers:
//!   1. `crate::indexer::chunker::strip_nul_bytes` -- the canonical
//!      stripper, now centralized in the chunker module.
//!   2. `crate::indexer::chunker::strip_nul_bytes_from_chunks` -- the
//!      per-chunk sweep, applied in `processor::process_file` after
//!      chunking and again in the embed-pool worker as defence in
//!      depth.

use pgmcp::indexer::chunker::{Chunk, strip_nul_bytes, strip_nul_bytes_from_chunks};
use pgmcp::indexer::claude_chunker::chunk_claude_jsonl;

#[test]
fn claude_chunker_returns_chunks_with_nul_then_strip_clears_them() {
    // The Rust string escape `\\u0000` produces the six characters
    // `\u0000` in the source bytes -- exactly the JSON escape for NUL.
    // serde_json then decodes that escape into a literal NUL byte in
    // the resulting Rust `String` value.
    let jsonl = "{\"type\":\"user\",\"message\":\"hello\\u0000world\"}";
    let mut chunks = chunk_claude_jsonl(jsonl);
    assert_eq!(chunks.len(), 1, "chunker produced {} chunks", chunks.len());
    assert!(
        chunks[0].content.contains('\0'),
        "decoded JSON should preserve NUL before strip; got {:?}",
        chunks[0].content
    );
    let changed = strip_nul_bytes_from_chunks(&mut chunks);
    assert!(changed);
    assert_eq!(chunks[0].content, "[user] helloworld");
}

#[test]
fn strip_nul_bytes_round_trips_clean_content_without_change() {
    let mut s = String::from("[user] no nul bytes here");
    assert!(!strip_nul_bytes(&mut s));
    assert_eq!(s, "[user] no nul bytes here");
}

#[test]
fn strip_nul_bytes_from_chunks_is_no_op_on_clean_chunks() {
    let jsonl = "{\"type\":\"user\",\"message\":\"just a regular message\"}";
    let mut chunks = chunk_claude_jsonl(jsonl);
    assert_eq!(chunks.len(), 1);
    assert!(!strip_nul_bytes_from_chunks(&mut chunks));
    assert_eq!(chunks[0].content, "[user] just a regular message");
}

#[test]
fn multiple_messages_with_nul_all_get_stripped() {
    let jsonl = concat!(
        "{\"type\":\"user\",\"message\":\"first\\u0000message\"}\n",
        "{\"type\":\"assistant\",\"message\":\"second\\u0000here\"}\n",
    );
    let mut chunks = chunk_claude_jsonl(jsonl);
    assert!(chunks.len() >= 1, "claude chunker produced chunks");
    let any_nul_before = chunks.iter().any(|c| c.content.contains('\0'));
    assert!(any_nul_before, "at least one chunk has a NUL pre-strip");
    let changed = strip_nul_bytes_from_chunks(&mut chunks);
    assert!(changed);
    for c in &chunks {
        assert!(!c.content.contains('\0'), "post-strip chunk: {:?}", c.content);
    }
}

#[test]
fn chunk_struct_strip_is_lossless_outside_of_nul() {
    // Construct a Chunk manually with mixed unicode + NUL. The NUL is
    // written as the Rust escape `\0` so the source file bytes contain
    // a literal 0x00; that's the chunker output shape we expect to
    // strip in the post-chunk sweep.
    let mut chunks = vec![Chunk {
        chunk_index: 0,
        content: "h\u{e9}llo\0w\u{f6}rld\u{1F600}".to_string(),
        start_line: 1,
        end_line: 1,
    }];
    let changed = strip_nul_bytes_from_chunks(&mut chunks);
    assert!(changed);
    assert_eq!(chunks[0].content, "h\u{e9}llow\u{f6}rld\u{1F600}");
}
