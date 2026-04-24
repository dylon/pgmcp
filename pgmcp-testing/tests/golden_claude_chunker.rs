//! Golden-file tests for `pgmcp::indexer::claude_chunker`.
//!
//! These tests freeze Claude's JSONL-parsing behaviour for both the
//! happy path (user/assistant messages only) and the mixed path
//! (with skip-types: `progress`, `file-history-snapshot`, plus
//! `tool_result` content extraction).
//!
//! Regenerate via `cargo run --release -p pgmcp-testing --bin
//! regen-goldens` when changes to `chunk_claude_jsonl` are intentional.

use pgmcp::indexer::chunker::Chunk;
use pgmcp::indexer::claude_chunker;
use pgmcp_testing::golden::assert_match_exact;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonlChunkerInput {
    content: String,
}

#[test]
fn session_basic_matches_golden() {
    assert_match_exact::<JsonlChunkerInput, Vec<Chunk>>("claude_chunker/session_basic", |input| {
        claude_chunker::chunk_claude_jsonl(&input.content)
    });
}

#[test]
fn session_mixed_types_matches_golden() {
    assert_match_exact::<JsonlChunkerInput, Vec<Chunk>>(
        "claude_chunker/session_mixed_types",
        |input| claude_chunker::chunk_claude_jsonl(&input.content),
    );
}
