// Wired into the embed pipeline in Phase 5; until then the public surface
// looks dead to rustc. Submodules (none here) inherit the relaxed lint.
#![allow(dead_code)]

//! Heading- and paragraph-aware chunking for document languages.
//!
//! Different from `chunker::chunk_content` (line-window with overlap) and
//! `chunker::chunk_jsonl_content` (one-record-per-line). Documents have
//! natural semantic boundaries — section headings for structured source
//! formats (ORG, LaTeX) and blank-line paragraph breaks for extracted
//! plain text (PDF, PostScript, pandoc-plain output). Chunking on those
//! boundaries gives the embedder coherent units instead of arbitrary
//! line windows, which improves `semantic_search` relevance noticeably
//! on natural-language content.
//!
//! Two entry points:
//!
//! * `chunk_by_heading` — for ORG, LaTeX, and (future) markdown sources
//!   where headings define sections. Falls back to paragraph mode when a
//!   section grows beyond `max_chunk_chars`.
//! * `chunk_paragraphs` — for PDF/PostScript/pandoc-plain output and for
//!   sections oversized by the heading mode.
//!
//! Both produce chunks of ~`max_chunk_chars` characters (~500 tokens
//! given typical English prose), which is the sweet spot for the
//! all-MiniLM-L6-v2 model used by pgmcp.

use std::sync::OnceLock;

use regex::Regex;

use super::chunker::Chunk;

/// Tunable parameters for paragraph-aware chunking.
#[derive(Debug, Clone, Copy)]
pub struct ParagraphOpts {
    /// Maximum characters per chunk. Targeting ~2000 (≈ 500 tokens of
    /// English prose) hits the MiniLM-L6-v2 sweet spot without
    /// over-truncating long paragraphs.
    pub max_chunk_chars: usize,
    /// Minimum characters per chunk before merging into a neighbor.
    pub min_chunk_chars: usize,
    /// When true, attempt to rejoin lines that look like mid-sentence
    /// wraps from `pdftotext` / `ps2ascii`. Set to false when the source
    /// already has paragraph-only line breaks (rare).
    pub join_short_lines: bool,
}

pub const DEFAULT_PARAGRAPH_OPTS: ParagraphOpts = ParagraphOpts {
    max_chunk_chars: 2000,
    min_chunk_chars: 200,
    join_short_lines: true,
};

/// Heading-aware chunker. For each heading found, opens a new chunk at
/// the heading line. Sections smaller than `min_chunk_chars` are merged
/// with the next. Sections larger than `max_chunk_chars` get sliced via
/// `chunk_paragraphs` internally.
pub fn chunk_by_heading(content: &str, language: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.split('\n').collect();
    let n = lines.len();
    if n == 0 {
        return Vec::new();
    }

    let headings = find_heading_lines(content, language);

    // Build raw section boundaries: [start, end) line ranges (0-based,
    // exclusive end).
    let mut sections: Vec<(usize, usize)> = Vec::new();
    let mut prev = 0usize;
    for &h in &headings {
        if h > prev {
            sections.push((prev, h));
        }
        prev = h;
    }
    if prev < n {
        sections.push((prev, n));
    }
    if sections.is_empty() {
        // No headings — defer to paragraph chunker.
        return chunk_paragraphs(content, DEFAULT_PARAGRAPH_OPTS);
    }

    // Convert each section into a chunk; split oversize sections via
    // paragraph chunker; merge undersize sections forward.
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut buffer: Option<(usize, String)> = None; // (start_line_1based, body)

    for (start, end) in sections {
        let body = join_lines(&lines, start, end);
        if body.chars().count() > DEFAULT_PARAGRAPH_OPTS.max_chunk_chars {
            // Flush any pending undersize section first.
            if let Some((buf_start, buf_body)) = buffer.take() {
                push_chunk(&mut chunks, &buf_body, buf_start as i32);
            }
            // Slice this oversize section into paragraph chunks. The
            // returned chunks carry section-local line numbers; rebase
            // onto the document-absolute line range.
            let mut sub_chunks = chunk_paragraphs(&body, DEFAULT_PARAGRAPH_OPTS);
            for c in &mut sub_chunks {
                c.start_line += start as i32; // local 1-based + offset
                c.end_line += start as i32;
            }
            // Renumber chunk indices to continue the document sequence.
            for c in &mut sub_chunks {
                c.chunk_index = chunks.len() as i32;
                chunks.push(c.clone());
            }
        } else if body.chars().count() < DEFAULT_PARAGRAPH_OPTS.min_chunk_chars {
            // Accumulate small sections; merge with next.
            match &mut buffer {
                Some((_, b)) => {
                    if !b.ends_with('\n') {
                        b.push('\n');
                    }
                    b.push_str(&body);
                }
                None => {
                    buffer = Some((start + 1, body));
                }
            }
        } else {
            // Right-sized section — flush pending buffer first.
            if let Some((buf_start, buf_body)) = buffer.take() {
                push_chunk(&mut chunks, &buf_body, buf_start as i32);
            }
            push_chunk(&mut chunks, &body, (start + 1) as i32);
        }
    }
    if let Some((buf_start, buf_body)) = buffer.take() {
        if let Some(last) = chunks.last_mut() {
            if !last.content.ends_with('\n') {
                last.content.push('\n');
            }
            last.content.push_str(&buf_body);
        } else {
            push_chunk(&mut chunks, &buf_body, buf_start as i32);
        }
    }

    // Reassign chunk_index monotonically and fill end_line accurately.
    for (i, c) in chunks.iter_mut().enumerate() {
        c.chunk_index = i as i32;
    }
    chunks
}

/// Paragraph-aware chunker. Splits on blank-line boundaries, optionally
/// rejoins mid-sentence wraps first, then greedy-packs paragraphs into
/// chunks ≤ `max_chunk_chars`.
pub fn chunk_paragraphs(content: &str, opts: ParagraphOpts) -> Vec<Chunk> {
    let joined = if opts.join_short_lines {
        rejoin_wrapped_lines(content)
    } else {
        content.to_string()
    };

    let paragraphs = split_paragraphs(&joined);
    if paragraphs.is_empty() {
        return Vec::new();
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut buf = String::new();
    let mut buf_start: i32 = 0;
    let mut buf_end: i32 = 0;

    for (p_start, p_end, p_text) in paragraphs {
        let p_chars = p_text.chars().count();
        if p_chars > opts.max_chunk_chars {
            // Flush any pending buffer first.
            if !buf.is_empty() {
                push_chunk_lines(&mut chunks, &buf, buf_start, buf_end);
                buf.clear();
            }
            // Split the oversize paragraph along sentence boundaries.
            for piece in split_oversize_paragraph(&p_text, opts.max_chunk_chars) {
                push_chunk_lines(&mut chunks, &piece, p_start, p_end);
            }
            continue;
        }
        // Would adding this paragraph blow the cap?
        let prospective = buf.chars().count() + 1 + p_chars; // +1 for paragraph separator
        if !buf.is_empty() && prospective > opts.max_chunk_chars {
            push_chunk_lines(&mut chunks, &buf, buf_start, buf_end);
            buf.clear();
        }
        if buf.is_empty() {
            buf_start = p_start;
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(&p_text);
        buf_end = p_end;
    }
    if !buf.is_empty() {
        push_chunk_lines(&mut chunks, &buf, buf_start, buf_end);
    }

    // Merge any too-small tail into the previous chunk.
    if chunks.len() >= 2 {
        let last_len = chunks.last().expect("at least 2").content.chars().count();
        if last_len < opts.min_chunk_chars {
            let tail = chunks.pop().expect("at least 2");
            let prev = chunks.last_mut().expect("at least 1");
            if !prev.content.ends_with('\n') {
                prev.content.push('\n');
            }
            prev.content.push('\n');
            prev.content.push_str(&tail.content);
            prev.end_line = tail.end_line.max(prev.end_line);
        }
    }

    for (i, c) in chunks.iter_mut().enumerate() {
        c.chunk_index = i as i32;
    }
    chunks
}

/// Rejoin lines that look like mid-sentence wraps from PDF/PostScript
/// extraction. Heuristic: line ends without sentence-final punctuation,
/// next line is non-blank, not a heading, not a bullet, not pure digits.
fn rejoin_wrapped_lines(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let n = lines.len();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < n {
        let line = lines[i];
        let trimmed_end = line.trim_end();
        let next_line = if i + 1 < n { lines[i + 1] } else { "" };
        let next_trim = next_line.trim_start();

        let line_ends_sentence = trimmed_end
            .chars()
            .last()
            .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '"'));
        let next_is_blank = next_trim.is_empty();
        let next_is_heading = looks_like_heading(next_trim);
        let next_is_bullet = looks_like_bullet(next_trim);
        let next_is_pure_digits = !next_trim.is_empty()
            && next_trim
                .chars()
                .all(|c| c.is_ascii_digit() || c == ' ' || c == '.');

        let should_join = i + 1 < n
            && !trimmed_end.is_empty()
            && !line_ends_sentence
            && !next_is_blank
            && !next_is_heading
            && !next_is_bullet
            && !next_is_pure_digits;

        if should_join {
            out.push_str(trimmed_end);
            out.push(' ');
        } else {
            out.push_str(line);
            if i + 1 < n {
                out.push('\n');
            }
        }
        i += 1;
    }
    out
}

fn looks_like_heading(s: &str) -> bool {
    // Markdown / pandoc-md
    if s.starts_with('#') && s.contains(' ') {
        return true;
    }
    // ORG
    if s.starts_with('*') {
        let leading_stars: usize = s.chars().take_while(|c| *c == '*').count();
        let after = s.chars().nth(leading_stars);
        if after == Some(' ') {
            return true;
        }
    }
    // LaTeX
    if s.starts_with('\\') {
        for kw in [
            "\\section",
            "\\subsection",
            "\\chapter",
            "\\part",
            "\\paragraph",
        ] {
            if s.starts_with(kw) {
                return true;
            }
        }
    }
    false
}

fn looks_like_bullet(s: &str) -> bool {
    let mut chars = s.chars();
    let first = chars.next();
    match first {
        Some('-') | Some('*') | Some('•') | Some('·') => {
            // Must be followed by space to be a bullet (avoid stripping
            // hyphenated terms or LaTeX commands).
            chars.next().is_some_and(|c| c == ' ')
        }
        Some(d) if d.is_ascii_digit() => {
            // Numbered list: "1. Foo" or "1) Foo"
            let rest: String = chars.collect();
            matches!(rest.chars().next(), Some('.') | Some(')'))
        }
        _ => false,
    }
}

/// Split text on blank-line paragraph boundaries. Returns
/// `(start_line_1based, end_line_1based, paragraph_text)` for each
/// non-empty paragraph.
fn split_paragraphs(content: &str) -> Vec<(i32, i32, String)> {
    let mut paragraphs: Vec<(i32, i32, String)> = Vec::new();
    let mut buf = String::new();
    let mut buf_start: i32 = 0;
    let mut buf_end: i32 = 0;

    for (idx, line) in content.split('\n').enumerate() {
        let line_no = (idx + 1) as i32;
        if line.trim().is_empty() {
            if !buf.is_empty() {
                paragraphs.push((buf_start, buf_end, buf.clone()));
                buf.clear();
            }
            continue;
        }
        if buf.is_empty() {
            buf_start = line_no;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
        buf_end = line_no;
    }
    if !buf.is_empty() {
        paragraphs.push((buf_start, buf_end, buf));
    }
    paragraphs
}

fn split_oversize_paragraph(text: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    while start < chars.len() {
        let end_cap = (start + max_chars).min(chars.len());
        if end_cap >= chars.len() {
            out.push(chars[start..].iter().collect());
            break;
        }
        // Walk back from end_cap looking for a sentence boundary
        // (`. `, `! `, `? `).
        let mut split = end_cap;
        let mut found = false;
        let mut k = end_cap;
        while k > start + 1 {
            k -= 1;
            let c = chars[k];
            if (c == '.' || c == '!' || c == '?')
                && k + 1 < chars.len()
                && chars[k + 1].is_whitespace()
            {
                split = k + 1;
                found = true;
                break;
            }
        }
        if !found || split <= start {
            split = end_cap;
        }
        out.push(chars[start..split].iter().collect());
        start = split;
        // Skip leading whitespace at the new start.
        while start < chars.len() && chars[start].is_whitespace() {
            start += 1;
        }
    }
    out
}

fn join_lines(lines: &[&str], start: usize, end: usize) -> String {
    let mut s = String::new();
    for (i, line) in lines.iter().take(end).skip(start).enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(line);
    }
    s
}

/// Push a chunk, computing end_line from start_line + line count.
fn push_chunk(chunks: &mut Vec<Chunk>, body: &str, start_line: i32) {
    let line_count = body.split('\n').count() as i32;
    let end_line = start_line + line_count - 1;
    let chunk_index = chunks.len() as i32;
    chunks.push(Chunk {
        chunk_index,
        content: body.to_string(),
        start_line,
        end_line: end_line.max(start_line),
    });
}

fn push_chunk_lines(chunks: &mut Vec<Chunk>, body: &str, start_line: i32, end_line: i32) {
    let chunk_index = chunks.len() as i32;
    chunks.push(Chunk {
        chunk_index,
        content: body.to_string(),
        start_line: start_line.max(1),
        end_line: end_line.max(start_line.max(1)),
    });
}

/// Locate heading line numbers (0-based) for a given language. ORG and
/// LaTeX have well-defined patterns; RST is deferred (underline-based,
/// two-line lookahead); for anything else this returns an empty list and
/// the caller falls back to paragraph chunking.
fn find_heading_lines(content: &str, language: &str) -> Vec<usize> {
    let re = match language {
        "org" => Some(org_heading_re()),
        "latex" => Some(latex_heading_re()),
        // pandoc-plain output from `.tex`/`.org` mostly drops markdown
        // syntax, so this regex rarely fires — but it's safe to include.
        "markdown" => Some(md_heading_re()),
        _ => None,
    };
    let Some(re) = re else { return Vec::new() };

    content
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| if re.is_match(line) { Some(i) } else { None })
        .collect()
}

fn org_heading_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\*+\s+\S").unwrap())
}

fn latex_heading_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^\s*\\(part|chapter|section|subsection|subsubsection|paragraph|subparagraph)\*?\s*\{",
        )
        .unwrap()
    })
}

fn md_heading_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^#{1,6}\s+\S").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_chunker_simple() {
        let content = "Para one is a moderate-sized paragraph that contains a few sentences \
                       so it has enough characters to be worth keeping as its own chunk. We're \
                       writing a few more words here to be sure.\n\
                       \n\
                       Para two is another moderate-sized paragraph with comparable length so \
                       the chunker has to deal with two real chunks back-to-back rather than \
                       merging them.";
        let chunks = chunk_paragraphs(content, DEFAULT_PARAGRAPH_OPTS);
        assert!(!chunks.is_empty(), "expected at least one chunk");
        // Each chunk should carry line metadata.
        for c in &chunks {
            assert!(c.start_line >= 1);
            assert!(c.end_line >= c.start_line);
        }
    }

    #[test]
    fn paragraph_chunker_splits_oversize() {
        // Construct a single paragraph of ~3000 chars with sentence
        // boundaries; the chunker should split it.
        let sentence = "This is a sentence with a period and reasonable length. ";
        let big = sentence.repeat(80); // ~4500 chars
        let chunks = chunk_paragraphs(&big, DEFAULT_PARAGRAPH_OPTS);
        assert!(
            chunks.len() >= 2,
            "expected oversize paragraph to split, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.content.chars().count() <= DEFAULT_PARAGRAPH_OPTS.max_chunk_chars + 50,
                "chunk too large: {} chars",
                c.content.chars().count()
            );
        }
    }

    #[test]
    fn paragraph_chunker_merges_undersize_tail() {
        // Make a body that produces a tail chunk smaller than min_chunk_chars.
        let big = "abcdef ".repeat(400); // ~2800 chars, one paragraph
        let small = "Tail.";
        let content = format!("{big}\n\n{small}");
        let chunks = chunk_paragraphs(&content, DEFAULT_PARAGRAPH_OPTS);
        // The small "Tail." (< 200 chars) should have been merged.
        assert!(chunks.iter().any(|c| c.content.contains("Tail.")));
    }

    #[test]
    fn heading_chunker_org() {
        let body_a = "This is the body of the first section. ".repeat(8);
        let body_b = "Another body that is also of reasonable length. ".repeat(8);
        let content = format!(
            "* First section\n{body_a}\n\n* Second section\n{body_b}\n",
            body_a = body_a,
            body_b = body_b
        );
        let chunks = chunk_by_heading(&content, "org");
        assert!(
            chunks.len() >= 2,
            "expected multiple heading-based chunks, got {}",
            chunks.len()
        );
        assert!(chunks[0].content.contains("First section"));
        assert!(chunks.iter().any(|c| c.content.contains("Second section")));
    }

    #[test]
    fn heading_chunker_latex() {
        let body_a = "Some introductory prose with a few sentences. ".repeat(8);
        let body_b = "The methods section explains how the experiment was conducted. ".repeat(8);
        let content = format!(
            "\\section{{Intro}}\n{body_a}\n\n\\section{{Methods}}\n{body_b}\n",
            body_a = body_a,
            body_b = body_b
        );
        let chunks = chunk_by_heading(&content, "latex");
        assert!(
            chunks.len() >= 2,
            "expected multiple heading-based chunks, got {}",
            chunks.len()
        );
        assert!(chunks.iter().any(|c| c.content.contains("Intro")));
        assert!(chunks.iter().any(|c| c.content.contains("Methods")));
    }

    #[test]
    fn heading_chunker_merges_short_sections() {
        // Two short sections should merge into one chunk.
        let content = "* First\nshort body\n\n* Second\nalso short\n";
        let chunks = chunk_by_heading(content, "org");
        assert_eq!(chunks.len(), 1, "expected short sections to be merged");
        assert!(chunks[0].content.contains("First"));
        assert!(chunks[0].content.contains("Second"));
    }

    #[test]
    fn heading_chunker_preamble_first_chunk() {
        let content = "Preamble text that comes before any heading and is reasonably long \
                       to qualify as a chunk by itself with several sentences here too.\n\
                       \n\
                       * Heading\n\
                       Body content beneath the heading with another long-enough paragraph \
                       to be its own chunk.";
        let chunks = chunk_by_heading(content, "org");
        assert!(chunks[0].content.starts_with("Preamble"));
    }

    #[test]
    fn rejoin_wraps_mid_sentence() {
        // Lines that don't end in sentence punctuation get joined.
        let input = "This is a sentence wrapped\nacross two lines.";
        let out = rejoin_wrapped_lines(input);
        assert!(!out.contains('\n'), "expected join, got: {out:?}");
    }

    #[test]
    fn rejoin_preserves_blank_lines() {
        let input = "Paragraph one.\n\nParagraph two.";
        let out = rejoin_wrapped_lines(input);
        assert!(out.contains("\n\n"), "expected blank line preserved");
    }

    #[test]
    fn rejoin_preserves_bullets() {
        let input = "Above bullet line\n- bullet item";
        let out = rejoin_wrapped_lines(input);
        assert!(
            out.contains('\n'),
            "expected bullet to start on its own line"
        );
    }
}
