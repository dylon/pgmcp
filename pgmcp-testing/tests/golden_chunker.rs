//! Golden-file tests for `pgmcp::indexer::chunker`.
//!
//! These tests assert that chunker output for a canonical input
//! matches a frozen fixture byte-for-byte. They catch silent drift
//! in chunking behaviour that invariant tests (line counts, range
//! validity) can't detect — e.g. off-by-one in overlap, changes in
//! CRLF handling, differences in content-slicing semantics.
//!
//! When the drift is intentional, regenerate via:
//!
//! ```text
//! cargo run --release -p pgmcp-testing --bin regen-goldens
//! ```
//!
//! and commit the updated `.postcard` files.

use pgmcp::indexer::chunker::{self, Chunk};
use pgmcp_testing::golden::assert_match_exact;
use serde::{Deserialize, Serialize};

/// Mirrors the generator-side struct in `regen_goldens.rs` so the
/// envelope deserialises into a symmetric shape. The two copies
/// must stay in lockstep — changing one without the other causes a
/// schema-mismatch panic on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkerInput {
    content: String,
    chunk_size_lines: usize,
    chunk_overlap_lines: usize,
}

/// Input for `chunk_jsonl_content` — no knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonlChunkerInput {
    content: String,
}

fn run_chunk(input: &ChunkerInput) -> Vec<Chunk> {
    chunker::chunk_content(
        &input.content,
        input.chunk_size_lines,
        input.chunk_overlap_lines,
    )
}

#[test]
fn short_rust_file_chunks_match_golden() {
    assert_match_exact::<ChunkerInput, Vec<Chunk>>("chunker/short_rust_file", run_chunk);
}

#[test]
fn long_rust_with_overlap_chunks_match_golden() {
    assert_match_exact::<ChunkerInput, Vec<Chunk>>("chunker/long_rust_with_overlap", run_chunk);
}

#[test]
fn single_line_chunks_match_golden() {
    assert_match_exact::<ChunkerInput, Vec<Chunk>>("chunker/single_line", run_chunk);
}

#[test]
fn crlf_content_chunks_match_golden() {
    assert_match_exact::<ChunkerInput, Vec<Chunk>>("chunker/crlf_content", run_chunk);
}

#[test]
fn jsonl_mixed_chunks_match_golden() {
    assert_match_exact::<JsonlChunkerInput, Vec<Chunk>>("chunker/jsonl_mixed", |input| {
        chunker::chunk_jsonl_content(&input.content)
    });
}
