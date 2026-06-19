//! In-process LaTeX → plain-text extraction. Replaces the `pandoc` subprocess
//! for `.tex` files; mirrors `office::extract_via_pandoc`'s contract.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use latex_parser::parse;

use super::super::normalize::normalize_extracted_text;
use super::super::{ExtractError, ExtractOptions, Extracted};
use super::render::{RenderOptions, to_plain_text};

/// Extract plain text from a LaTeX (`.tex`) file using the in-process
/// `latex-parser`.
///
/// Unlike the pandoc path this cannot fail with `ToolMissing` (no external
/// tool) and never hard-fails on imperfect LaTeX (the parser is error-tolerant).
/// `parse` is fuzz-proven panic-free, but a defensive inner `catch_unwind` lets
/// any future parser/renderer regression degrade to the raw source rather than
/// unwind into the embed pool's outer catch — which would skip the file.
pub fn extract(path: &Path, opts: &ExtractOptions) -> Result<Option<Extracted>, ExtractError> {
    let bytes = std::fs::read(path).map_err(ExtractError::Io)?;
    Ok(Some(extract_bytes(&bytes, opts, Some(path))))
}

/// The I/O-free core: render already-read source bytes. Separated so tests can
/// exercise the renderer without a temp file.
fn extract_bytes(bytes: &[u8], opts: &ExtractOptions, path_for_log: Option<&Path>) -> Extracted {
    let source_size_bytes = bytes.len() as u64;

    // Bound the parsed input by the extracted-bytes cap (source bytes are a
    // ceiling on output). Truncate on a UTF-8 char boundary.
    let max = opts.max_extracted_bytes;
    let (slice, truncated_in) = if bytes.len() > max {
        (&bytes[..floor_char_boundary(bytes, max)], true)
    } else {
        (bytes, false)
    };
    let src = String::from_utf8_lossy(slice);

    let rendered = catch_unwind(AssertUnwindSafe(|| {
        let doc = parse(&src).expect("recovery-mode parse never errors");
        to_plain_text(&doc, src.len(), &RenderOptions::default())
    }));
    let text_raw = match rendered {
        Ok(text) => text,
        Err(_) => {
            tracing::error!(
                path = ?path_for_log,
                "latex render panicked; falling back to raw source"
            );
            src.into_owned()
        }
    };

    let text = normalize_extracted_text(&text_raw);
    let truncated = truncated_in || text.len() >= opts.max_extracted_bytes;
    Extracted {
        text,
        truncated,
        source_size_bytes,
    }
}

/// Round `idx` down to a UTF-8 char boundary within `bytes` (continuation bytes
/// match `0b10xxxxxx`).
fn floor_char_boundary(bytes: &[u8], idx: usize) -> usize {
    let mut i = idx.min(bytes.len());
    while i > 0 && (bytes[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_str(src: &str) -> String {
        extract_bytes(src.as_bytes(), &ExtractOptions::default(), None).text
    }

    #[test]
    fn section_without_brace_keeps_prose() {
        // The oslf.rholang.tex failure class: pandoc exits 64 here and pgmcp
        // skipped the file. We must return non-empty prose.
        let src = "\\section\n  The semantics implied by these rules.";
        let text = extract_str(src);
        assert!(
            text.contains("semantics implied by these rules"),
            "prose lost: {text:?}"
        );
    }

    #[test]
    fn math_renders_unicode_not_raw_tex() {
        let src = r"Let $x^2 + \alpha \le \frac{a}{b}$ hold.";
        let text = extract_str(src);
        assert!(text.contains('≤'), "operator not rendered: {text:?}");
        assert!(text.contains('α'), "greek not rendered: {text:?}");
        assert!(text.contains("a/b"), "fraction not rendered: {text:?}");
        assert!(!text.contains("\\frac"), "raw TeX leaked: {text:?}");
    }

    #[test]
    fn verbatim_code_is_emitted_literally() {
        let src = "\\begin{verbatim}\nfn main() { let x = $y; }\n\\end{verbatim}";
        let text = extract_str(src);
        assert!(text.contains("fn main()"), "code body lost: {text:?}");
        assert!(text.contains("let x = $y;"), "code body altered: {text:?}");
    }

    #[test]
    fn accents_compose_to_unicode() {
        let text = extract_str(r#"na\"ive caf\'e"#);
        assert!(text.contains("naïve"), "diaeresis not composed: {text:?}");
        assert!(text.contains("café"), "acute not composed: {text:?}");
    }

    #[test]
    fn prose_commands_unwrapped_metadata_dropped() {
        let src = r"\section{Intro}\label{sec:intro} See \textbf{this} \cite{foo}.";
        let text = extract_str(src);
        assert!(text.contains("Intro"));
        assert!(text.contains("this"));
        assert!(!text.contains("sec:intro"), "label leaked: {text:?}");
        assert!(!text.contains("foo"), "cite key leaked: {text:?}");
    }

    #[test]
    fn truncation_sets_flag() {
        let big = "x ".repeat(100);
        let opts = ExtractOptions {
            max_extracted_bytes: 20,
            ..ExtractOptions::default()
        };
        let out = extract_bytes(big.as_bytes(), &opts, None);
        assert!(out.truncated);
        assert_eq!(out.source_size_bytes, big.len() as u64);
    }
}
