//! Token-efficient post-extraction text normalization.
//!
//! `normalize_extracted_text` is called by every document extractor before
//! the result is returned. Its purpose is to strip artifacts that bloat
//! tokens (form-feeds, page-number lines, ligatures, hyphenated line
//! breaks) and collapse whitespace, so the same content can be stored and
//! delivered to MCP clients in the smallest UTF-8 representation that
//! still preserves meaning.
//!
//! The pass is idempotent — calling it twice on the same input is a no-op.

use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// Apply the full normalization pipeline. Order matters; see comments.
pub fn normalize_extracted_text(raw: &str) -> String {
    // 1. NFKC: collapse ligatures (ﬁ → fi), normalize fancy quotes, etc.
    let nfkc: String = raw.nfkc().collect();

    // 2. Drop control characters except \n and \t. Substitute NBSP / other
    //    space-equivalent whitespace with regular space at the same time.
    let mut step2 = String::with_capacity(nfkc.len());
    for ch in nfkc.chars() {
        match ch {
            '\n' | '\t' => step2.push(ch),
            c if c.is_control() => {} // drop \r, \f, \0, etc.
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => step2.push(' '),
            c => step2.push(c),
        }
    }

    // 3. Strip page-number artifacts (lone numeric lines, "Page N", "Page N of M").
    let page_re = page_number_regex();
    let mut step3 = String::with_capacity(step2.len());
    for (i, line) in step2.split('\n').enumerate() {
        if page_re.is_match(line) {
            // Drop the line but preserve the newline structure if the line
            // we're dropping is internal. (We re-emit a blank line so
            // surrounding paragraph boundaries aren't merged accidentally.)
            if i > 0 {
                step3.push('\n');
            }
            continue;
        }
        if i > 0 {
            step3.push('\n');
        }
        step3.push_str(line);
    }

    // 4. Dehyphenate line-break-split words.
    let step4 = dehyphenate(&step3);

    // 5. Right-trim each line and collapse intra-line whitespace runs
    //    while preserving leading indentation. Must come BEFORE the
    //    blank-line collapse: a line containing only whitespace would
    //    otherwise survive the collapse pass, then become empty here,
    //    breaking idempotency (the second pass would collapse what the
    //    first pass left behind).
    let mut step5 = String::with_capacity(step4.len());
    let mut first = true;
    for line in step4.split('\n') {
        if !first {
            step5.push('\n');
        }
        first = false;
        step5.push_str(&collapse_intra_line_whitespace(line.trim_end()));
    }

    // 6. Collapse runs of 3+ blank lines down to 2.
    let collapse_re = excess_blank_lines_regex();
    collapse_re.replace_all(&step5, "\n\n").into_owned()
}

fn page_number_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*(?:\d{1,4}|[Pp]age\s+\d+(?:\s+of\s+\d+)?)\s*$").unwrap())
}

fn excess_blank_lines_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\n{3,}").unwrap())
}

/// Join words split by a hard hyphen at end-of-line.
///
/// Heuristic: a line ending in `-` followed by a line whose first
/// non-whitespace character is a lowercase letter is treated as a wrapped
/// word — the hyphen and newline are removed and the next-line indent is
/// stripped. We deliberately do NOT join when the next line starts with
/// an uppercase letter (likely a compound proper noun like `Foo-Bar`).
fn dehyphenate(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let lines: Vec<&str> = input.split('\n').collect();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let line = lines[i];
        if i + 1 < n {
            let stripped = line.trim_end_matches([' ', '\t']);
            if let Some(before_hyphen) = stripped.strip_suffix('-') {
                let last_is_letter = before_hyphen
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphabetic());
                let next_line = lines[i + 1];
                let next_trim = next_line.trim_start();
                let next_first = next_trim.chars().next();
                let next_is_lower_alpha =
                    next_first.is_some_and(|c| c.is_alphabetic() && c.is_lowercase());
                if last_is_letter && next_is_lower_alpha {
                    // Drop the trailing hyphen + glue with next line's
                    // leading-whitespace stripped.
                    out.push_str(before_hyphen);
                    out.push_str(next_trim);
                    // Continue scanning at the line after next.
                    i += 2;
                    if i < n {
                        out.push('\n');
                    }
                    continue;
                }
            }
        }
        out.push_str(line);
        if i + 1 < n {
            out.push('\n');
        }
        i += 1;
    }
    out
}

fn collapse_intra_line_whitespace(line: &str) -> String {
    // Preserve leading whitespace verbatim; collapse runs of 2+ spaces in
    // the body to a single space.
    let leading_len = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let (lead, body) = line.split_at(
        line.char_indices()
            .nth(leading_len)
            .map(|(i, _)| i)
            .unwrap_or(line.len()),
    );
    let mut out = String::with_capacity(line.len());
    out.push_str(lead);
    let mut last_was_space = false;
    for c in body.chars() {
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
            out.push(' ');
        } else {
            last_was_space = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfkc_collapses_ligatures() {
        let out = normalize_extracted_text("ﬁre and ﬂame");
        assert_eq!(out, "fire and flame");
    }

    #[test]
    fn dehyphenates_split_word() {
        let input = "infor-\nmation theory";
        let out = normalize_extracted_text(input);
        assert!(
            out.contains("information"),
            "expected dehyphenation; got {:?}",
            out
        );
        assert!(!out.contains("-\n"), "should not retain hyphen-newline");
    }

    #[test]
    fn does_not_dehyphenate_when_next_is_capital() {
        // "Foo-\nBar" likely means compound proper noun "Foo-Bar" with a
        // genuine hyphen — do NOT join.
        let out = normalize_extracted_text("Foo-\nBar");
        assert!(out.contains('-'));
        // The newline survives a non-join.
        assert!(out.contains('\n'));
    }

    #[test]
    fn strips_lone_numeric_lines() {
        let input = "Body text\n   42  \nMore body";
        let out = normalize_extracted_text(input);
        assert!(!out.contains("42"));
        assert!(out.contains("Body text"));
        assert!(out.contains("More body"));
    }

    #[test]
    fn strips_page_n_of_m() {
        let input = "Header\nPage 3 of 12\nBody";
        let out = normalize_extracted_text(input);
        assert!(!out.contains("Page 3"));
    }

    #[test]
    fn drops_control_chars() {
        let input = "Hello\x0Cworld\rOK";
        let out = normalize_extracted_text(input);
        assert!(!out.contains('\x0C'));
        assert!(!out.contains('\r'));
        assert!(out.contains("Hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn nbsp_to_space() {
        let input = "non\u{00A0}breaking\u{00A0}space";
        let out = normalize_extracted_text(input);
        assert_eq!(out, "non breaking space");
    }

    #[test]
    fn collapses_three_or_more_blank_lines() {
        let input = "A\n\n\n\n\nB";
        let out = normalize_extracted_text(input);
        assert_eq!(out, "A\n\nB");
    }

    #[test]
    fn collapses_intra_line_spaces() {
        let input = "Hello    world";
        let out = normalize_extracted_text(input);
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn preserves_leading_indent() {
        let input = "  code-like indent\n    deeper";
        let out = normalize_extracted_text(input);
        assert_eq!(out, "  code-like indent\n    deeper");
    }

    #[test]
    fn idempotent_on_already_normalized() {
        let input = "Hello world\n\nNext paragraph";
        let once = normalize_extracted_text(input);
        let twice = normalize_extracted_text(&once);
        assert_eq!(once, twice);
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn prop_idempotent(s in "[A-Za-z0-9 \n\t.,;:!?-]{0,512}") {
            let once = normalize_extracted_text(&s);
            let twice = normalize_extracted_text(&once);
            prop_assert_eq!(once, twice);
        }
    }
}
