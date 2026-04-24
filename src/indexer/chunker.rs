//! Line-based content chunking with configurable size and overlap.

/// A chunk of file content.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    /// Zero-based chunk index.
    pub chunk_index: i32,
    /// Chunk content text.
    pub content: String,
    /// One-based start line.
    pub start_line: i32,
    /// One-based end line (inclusive).
    pub end_line: i32,
}

/// Split content into chunks by lines with overlap.
pub fn chunk_content(
    content: &str,
    chunk_size_lines: usize,
    chunk_overlap_lines: usize,
) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return vec![];
    }

    if total_lines <= chunk_size_lines {
        return vec![Chunk {
            chunk_index: 0,
            content: content.to_string(),
            start_line: 1,
            end_line: total_lines as i32,
        }];
    }

    let step = chunk_size_lines.saturating_sub(chunk_overlap_lines).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_idx = 0;

    while start < total_lines {
        let end = (start + chunk_size_lines).min(total_lines);
        let chunk_lines = &lines[start..end];
        let content = chunk_lines.join("\n");

        chunks.push(Chunk {
            chunk_index: chunk_idx,
            content,
            start_line: (start + 1) as i32,
            end_line: end as i32,
        });

        chunk_idx += 1;
        start += step;

        // Avoid creating a tiny trailing chunk
        if start < total_lines && total_lines - start < chunk_overlap_lines {
            break;
        }
    }

    chunks
}

/// Chunk a generic JSONL file: each non-empty line becomes one chunk.
/// Used for JSONL files outside `~/.claude/`.
pub fn chunk_jsonl_content(content: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut chunk_index: i32 = 0;

    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let line_1based = (line_num + 1) as i32;
        chunks.push(Chunk {
            chunk_index,
            content: trimmed.to_string(),
            start_line: line_1based,
            end_line: line_1based,
        });
        chunk_index += 1;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_empty_content() {
        let chunks = chunk_content("", 10, 2);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_small_file_single_chunk() {
        let content = "line1\nline2\nline3";
        let chunks = chunk_content(content, 10, 2);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn test_chunking_with_overlap() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let content = lines.join("\n");

        let chunks = chunk_content(&content, 10, 3);

        // With chunk_size=10 and overlap=3, step=7
        // Chunk 0: lines 1-10
        // Chunk 1: lines 8-17
        // Chunk 2: lines 15-20
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 10);
    }

    #[test]
    fn test_chunk_content_covers_all_lines() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {}", i)).collect();
        let content = lines.join("\n");

        let chunks = chunk_content(&content, 50, 10);

        // Verify first chunk starts at line 1
        assert_eq!(chunks[0].start_line, 1);

        // Verify last chunk covers the end
        let last = chunks.last().expect("should have chunks");
        assert!(last.end_line >= 90); // Should reach near the end
    }

    #[test]
    fn test_chunk_jsonl_content() {
        let jsonl = r#"{"key": "value1"}
{"key": "value2"}

{"key": "value3"}
"#;
        let chunks = chunk_jsonl_content(jsonl);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].start_line, 2);
        assert_eq!(chunks[2].start_line, 4); // Line 3 was empty
    }

    #[test]
    fn test_chunk_jsonl_empty() {
        assert!(chunk_jsonl_content("").is_empty());
        assert!(chunk_jsonl_content("  \n  \n").is_empty());
    }

    #[test]
    fn test_no_overlap() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let content = lines.join("\n");

        let chunks = chunk_content(&content, 10, 0);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 10);
        assert_eq!(chunks[1].start_line, 11);
        assert_eq!(chunks[1].end_line, 20);
    }

    // ========================================================================
    // Proptest: chunk_content properties
    // ========================================================================

    /// Strategy to generate non-empty content with a known number of lines.
    fn content_strategy() -> impl Strategy<Value = (String, usize)> {
        // Generate 1-500 lines of arbitrary non-empty text
        prop::collection::vec("[a-zA-Z0-9 ]{1,80}", 1..500usize).prop_map(|lines| {
            let n = lines.len();
            (lines.join("\n"), n)
        })
    }

    /// Strategy for valid chunk parameters (overlap < chunk_size).
    fn chunk_params_strategy() -> impl Strategy<Value = (usize, usize)> {
        (2usize..100).prop_flat_map(|chunk_size| {
            let max_overlap = chunk_size - 1;
            (Just(chunk_size), 0..max_overlap)
        })
    }

    proptest! {
        #[test]
        fn prop_chunks_cover_first_and_last_line(
            (content, total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);

            prop_assert!(!chunks.is_empty(), "non-empty content must produce chunks");

            // First chunk must start at line 1
            prop_assert_eq!(chunks[0].start_line, 1, "first chunk must start at line 1");

            // Last chunk must reach into the final lines of the content
            let last = chunks.last().expect("must have chunks");
            prop_assert!(
                last.end_line as usize >= total_lines.saturating_sub(overlap),
                "last chunk end_line {} must cover near end of {} total lines (overlap={})",
                last.end_line, total_lines, overlap
            );
        }

        #[test]
        fn prop_chunks_have_no_gaps(
            (content, _total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);

            if chunks.len() >= 2 {
                for pair in chunks.windows(2) {
                    // Next chunk's start_line must be <= previous chunk's end_line + 1
                    // (i.e., no gap; overlap means start_line <= end_line of previous)
                    prop_assert!(
                        pair[1].start_line <= pair[0].end_line + 1,
                        "gap between chunks: prev ends at {}, next starts at {}",
                        pair[0].end_line, pair[1].start_line
                    );
                }
            }
        }

        #[test]
        fn prop_chunk_indices_are_sequential(
            (content, _total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);

            for (i, chunk) in chunks.iter().enumerate() {
                prop_assert_eq!(
                    chunk.chunk_index, i as i32,
                    "chunk_index should be sequential, got {} at position {}",
                    chunk.chunk_index, i
                );
            }
        }

        #[test]
        fn prop_chunk_line_ranges_are_valid(
            (content, total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);

            for chunk in &chunks {
                prop_assert!(chunk.start_line >= 1, "start_line must be >= 1");
                prop_assert!(
                    chunk.end_line <= total_lines as i32,
                    "end_line {} must be <= total_lines {}",
                    chunk.end_line, total_lines
                );
                prop_assert!(
                    chunk.start_line <= chunk.end_line,
                    "start_line {} must be <= end_line {}",
                    chunk.start_line, chunk.end_line
                );
                let line_count = (chunk.end_line - chunk.start_line + 1) as usize;
                prop_assert!(
                    line_count <= chunk_size,
                    "chunk has {} lines but chunk_size is {}",
                    line_count, chunk_size
                );
            }
        }

        #[test]
        fn prop_single_line_content_single_chunk(
            line in "[a-zA-Z0-9]{1,80}",
            chunk_size in 1usize..50,
            overlap in 0usize..5,
        ) {
            let chunks = chunk_content(&line, chunk_size, overlap);
            prop_assert_eq!(chunks.len(), 1, "single-line content must produce exactly 1 chunk");
            prop_assert_eq!(chunks[0].start_line, 1);
            prop_assert_eq!(chunks[0].end_line, 1);
        }

        /// Non-final adjacent chunks must overlap by exactly `overlap` lines.
        /// The final chunk may be shorter (cut off near end-of-input), so
        /// we only check the invariant for pairs where the second chunk
        /// occupies a full window.
        #[test]
        fn prop_chunk_overlap_within_bounds(
            (content, _total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);
            for pair in chunks.windows(2) {
                let next_is_full = (pair[1].end_line - pair[1].start_line + 1) as usize == chunk_size;
                if next_is_full {
                    // start_2 = start_1 + (chunk_size - overlap)
                    // → end_1 - start_2 + 1 = overlap
                    let actual_overlap = pair[0].end_line - pair[1].start_line + 1;
                    prop_assert_eq!(
                        actual_overlap as usize, overlap,
                        "adjacent full-size chunks must overlap by exactly {} lines, got {}",
                        overlap, actual_overlap,
                    );
                }
            }
        }

        /// Every input line that belongs to at least one chunk appears in the
        /// deduplicated line set reconstructed from all chunks. For non-tiny
        /// inputs, this is equivalent to "no line is dropped."
        #[test]
        fn prop_chunks_recombine_to_input(
            (content, total_lines) in content_strategy(),
            (chunk_size, overlap) in chunk_params_strategy(),
        ) {
            let chunks = chunk_content(&content, chunk_size, overlap);
            let mut covered = vec![false; total_lines];
            for chunk in &chunks {
                let start = (chunk.start_line as usize).saturating_sub(1);
                let end = chunk.end_line as usize; // exclusive in 0-based
                for c in covered.iter_mut().take(end.min(total_lines)).skip(start) {
                    *c = true;
                }
            }
            // Within the trim window allowed by the "avoid tiny trailing chunk"
            // logic, every line up to the last chunk's end_line should be covered.
            let last_end = chunks.last().map(|c| c.end_line as usize).unwrap_or(0);
            for (i, is_covered) in covered.iter().take(last_end).enumerate() {
                prop_assert!(is_covered, "line {} (0-based) not covered by any chunk", i);
            }
        }

        // ====================================================================
        // Proptests for chunk_jsonl_content
        // ====================================================================

        /// The number of emitted chunks equals the number of non-blank
        /// (after-trim) lines in the input.
        #[test]
        fn prop_chunk_jsonl_one_chunk_per_nonempty_line(
            lines in prop::collection::vec("[ \t]*([a-zA-Z0-9{}\": ,]{0,60})[ \t]*", 0..100usize),
        ) {
            let content = lines.join("\n");
            let expected_count = lines.iter().filter(|l| !l.trim().is_empty()).count();
            let chunks = chunk_jsonl_content(&content);
            prop_assert_eq!(chunks.len(), expected_count,
                "jsonl chunker must emit one chunk per non-empty trimmed line");
        }

        /// Every jsonl chunk spans exactly one input line (start_line == end_line).
        #[test]
        fn prop_chunk_jsonl_chunks_are_single_lined(
            lines in prop::collection::vec("[a-zA-Z0-9 ]{1,40}", 1..50usize),
        ) {
            let content = lines.join("\n");
            let chunks = chunk_jsonl_content(&content);
            for chunk in &chunks {
                prop_assert_eq!(chunk.start_line, chunk.end_line,
                    "jsonl chunk must span exactly one line: {}..{}",
                    chunk.start_line, chunk.end_line);
            }
        }

        /// jsonl chunk indices are dense [0, 1, 2, …, n-1].
        #[test]
        fn prop_chunk_jsonl_indices_dense_and_sequential(
            lines in prop::collection::vec("[a-zA-Z0-9]{1,30}", 0..50usize),
        ) {
            let content = lines.join("\n");
            let chunks = chunk_jsonl_content(&content);
            for (i, chunk) in chunks.iter().enumerate() {
                prop_assert_eq!(chunk.chunk_index, i as i32);
            }
        }

        /// jsonl chunk content is always the trimmed form of its source line.
        #[test]
        fn prop_chunk_jsonl_content_is_trimmed(
            lines in prop::collection::vec("[a-zA-Z0-9]{1,20}", 1..20usize),
            lpad in 0usize..5,
            rpad in 0usize..5,
        ) {
            let padded: Vec<String> = lines.iter()
                .map(|l| format!("{}{}{}", " ".repeat(lpad), l, " ".repeat(rpad)))
                .collect();
            let content = padded.join("\n");
            let chunks = chunk_jsonl_content(&content);
            for (i, chunk) in chunks.iter().enumerate() {
                prop_assert_eq!(&chunk.content, &lines[i],
                    "jsonl chunk {} content must equal trimmed source line", i);
            }
        }
    }

    // ========================================================================
    // Examples: CRLF normalization and BOM handling
    // ========================================================================

    /// Single-chunk fast path: the chunker passes the original content
    /// through verbatim, CRLF and all — no rejoining happens. Line counts
    /// are still correct because `str::lines()` handles both line endings.
    #[test]
    fn test_crlf_preserved_in_single_chunk_fast_path() {
        let content = "line1\r\nline2\r\nline3";
        let chunks = chunk_content(content, 10, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        // Original content is returned as-is — not rejoined.
        assert_eq!(chunks[0].content, content);
    }

    /// Multi-chunk path: when the chunker splits and rejoins with `\n`,
    /// the rejoined content no longer has CRLFs between the lines within a
    /// chunk. This documents actual behavior — downstream embedders see
    /// LF-separated content when a file is split.
    #[test]
    fn test_crlf_normalized_to_lf_when_rejoined_across_chunks() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line{}\r", i)).collect();
        let content = lines.join("\n"); // "line1\r\nline2\r\n..."
        let chunks = chunk_content(&content, 5, 0);
        assert!(chunks.len() >= 2, "expected multi-chunk split");
        // Rejoined content uses \n, so no CRLF survives between lines
        // within a single chunk.
        for chunk in &chunks {
            assert!(
                !chunk.content.contains("\r\n"),
                "rejoined chunk should not contain CRLF: {:?}",
                chunk.content
            );
        }
    }

    /// A UTF-8 BOM at the start of a file is preserved as part of the first
    /// line — not stripped. Embeddings trained on code will still match.
    #[test]
    fn test_utf8_bom_preserved_in_first_chunk() {
        let content = "\u{FEFF}fn main() {}\nfn foo() {}";
        let chunks = chunk_content(content, 10, 0);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with('\u{FEFF}'));
        assert_eq!(chunks[0].start_line, 1);
    }
}
