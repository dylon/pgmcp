//! Shared comment-stripping for regex-based formal-verification backends
//! (ADR-025).
//!
//! Line-anchored declaration regexes (`(?m)^\s*Theorem\s+(name)`, etc.) happily
//! match a keyword inside a comment, so a `(* Theorem fake … *)` would emit a
//! phantom symbol. This blanks comment spans — replacing every non-newline byte
//! with a space — BEFORE the regexes run, while **preserving byte offsets and
//! line numbers** (so `line_of(byte_offset)` stays correct and captured names
//! outside comments are byte-identical). Used by the Coq backend (fixing its
//! documented leak) and the Isabelle / Metamath / Why3 / Tamarin backends.

/// Comment syntax of an FV language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentStyle {
    /// Nested `(* … *)` block comments (Coq/Rocq, Isabelle, Why3).
    CoqBlock,
    /// Metamath `$( … $)` comments (not nested).
    Metamath,
    /// Tamarin: `/* … */` block (not nested) + `//` line comments (C-style).
    CStyle,
}

/// Return a copy of `src` with comment spans blanked. Byte length, byte offsets,
/// and newlines are preserved exactly; only non-newline bytes inside comments
/// become ASCII spaces.
pub fn strip_comments_preserving_lines(src: &str, style: CommentStyle) -> String {
    match style {
        CommentStyle::CoqBlock => strip_block(src, b"(*", b"*)", true, false),
        CommentStyle::Metamath => strip_block(src, b"$(", b"$)", false, false),
        CommentStyle::CStyle => strip_block(src, b"/*", b"*/", false, true),
    }
}

/// Blank `open … close` spans. `nested` allows depth > 1 (Coq). `line_comments`
/// also blanks `// …` to end-of-line (C-style). Operates on bytes; comment
/// markers are ASCII and never split a multi-byte char, so blanking whole spans
/// keeps the result valid UTF-8.
fn strip_block(src: &str, open: &[u8], close: &[u8], nested: bool, line_comments: bool) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    let mut depth = 0usize;
    let blank = |out: &mut [u8], from: usize, len: usize| {
        for byte in out.iter_mut().skip(from).take(len) {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i..].starts_with(open) {
            if depth == 0 || nested {
                depth += 1;
            }
            blank(&mut out, i, open.len());
            i += open.len();
            continue;
        }
        if depth > 0 && b[i..].starts_with(close) {
            depth = depth.saturating_sub(1);
            blank(&mut out, i, close.len());
            i += close.len();
            continue;
        }
        if depth == 0 && line_comments && b[i..].starts_with(b"//") {
            // Blank to (not including) the newline.
            let mut j = i;
            while j < b.len() && b[j] != b'\n' {
                out[j] = b' ';
                j += 1;
            }
            i = j;
            continue;
        }
        if depth > 0 {
            blank(&mut out, i, 1);
        }
        i += 1;
    }
    String::from_utf8(out).expect("blanking comment bytes to spaces preserves valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coq_nested_block_is_blanked_offsets_preserved() {
        let src = "Theorem real : True.\n(* Theorem fake (* nested *) here *)\nLemma also : True.";
        let out = strip_comments_preserving_lines(src, CommentStyle::CoqBlock);
        assert_eq!(out.len(), src.len(), "byte length must be preserved");
        // newlines preserved → same line count / offsets.
        assert_eq!(
            out.bytes().filter(|&b| b == b'\n').count(),
            src.bytes().filter(|&b| b == b'\n').count()
        );
        assert!(out.contains("Theorem real"), "code outside comments intact");
        assert!(out.contains("Lemma also"));
        assert!(
            !out.contains("fake"),
            "comment text must be blanked: {out:?}"
        );
        assert!(!out.contains("nested"));
    }

    #[test]
    fn metamath_comment_is_blanked() {
        let src = "axiom1 $a |- ph $.\n$( comment with axiom2 keyword $)\nthm2 $p |- ps $.";
        let out = strip_comments_preserving_lines(src, CommentStyle::Metamath);
        assert_eq!(out.len(), src.len());
        assert!(out.contains("axiom1"));
        assert!(out.contains("thm2"));
        assert!(!out.contains("axiom2"), "metamath comment must be blanked");
    }

    #[test]
    fn cstyle_block_and_line_comments_blanked() {
        let src = "rule Real:\n// rule Fake1\n/* rule Fake2 */\nlemma L:";
        let out = strip_comments_preserving_lines(src, CommentStyle::CStyle);
        assert_eq!(out.len(), src.len());
        assert!(out.contains("rule Real"));
        assert!(out.contains("lemma L"));
        assert!(!out.contains("Fake1"));
        assert!(!out.contains("Fake2"));
    }

    #[test]
    fn unicode_inside_comment_stays_valid_utf8() {
        let src = "Theorem t : True.\n(* ∀x∈ℝ, x=x — note *)\nQed.";
        let out = strip_comments_preserving_lines(src, CommentStyle::CoqBlock);
        assert_eq!(out.len(), src.len());
        assert!(out.contains("Theorem t"));
        assert!(!out.contains('∀'));
    }
}
