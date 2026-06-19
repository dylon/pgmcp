//! Pure doc-comment extraction + identifier redaction (strategy **B** ground
//! truth).
//!
//! The CodeSearchNet protocol (Husain et al. 2019) uses a code unit's
//! natural-language doc-comment as the query and the code itself as the gold
//! target. The catch is **leakage**: if the doc-comment is physically inside
//! the embedded chunk, retrieval is trivial. [`extract_leading_docstring`]
//! therefore returns both the doc text (the query) **and** the chunk body with
//! that doc removed ([`DocExtraction::body_without_doc`]) — the campaign
//! re-embeds the body so the stored vector never saw the query (the M1 control).
//! [`redact_identifiers`] additionally strips identifier tokens for the M3
//! variant, which isolates real semantics from identifier echo.
//!
//! These functions are pure and golden-tested (`regen-goldens`), so the
//! leakage-control logic is verified independently of any database.

use serde::{Deserialize, Serialize};

/// A leading doc-comment split from its code body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocExtraction {
    /// The natural-language doc text, comment markers stripped, lines joined
    /// with `\n`. This becomes the query.
    pub doc_text: String,
    /// The chunk content with the leading doc-comment removed. The M1 control
    /// re-embeds this so the stored vector never contains the query text.
    pub body_without_doc: String,
}

/// Extract a leading doc-comment from a code chunk, returning `None` unless the
/// chunk **begins** with a doc-comment (after optional blank lines) **and** has
/// non-empty code after it. Restricting to leading docs keeps the (query, gold)
/// pairing unambiguous: the doc describes the code in the same chunk.
pub fn extract_leading_docstring(content: &str, language: &str) -> Option<DocExtraction> {
    match language {
        "rust" => extract_rust(content),
        "python" => extract_python(content),
        _ => None,
    }
}

/// The first paragraph of a doc (up to the first blank line). CodeSearchNet uses
/// the first paragraph as the query; the rest is often `# Examples` / `# Panics`
/// boilerplate that dilutes the semantic signal.
pub fn first_paragraph(doc: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in doc.lines() {
        if line.trim().is_empty() {
            if !out.is_empty() {
                break;
            }
            continue;
        }
        out.push(line.trim());
    }
    out.join(" ")
}

/// Is this trimmed line a Rust doc-comment line (`///` outer or `//!` inner),
/// excluding the non-doc `////` form?
fn is_rust_doc_line(trimmed: &str) -> bool {
    (trimmed.starts_with("///") && !trimmed.starts_with("////")) || trimmed.starts_with("//!")
}

/// Strip the `///` / `//!` marker and one optional leading space from a Rust
/// doc line.
fn strip_rust_doc_marker(trimmed: &str) -> &str {
    let rest = if let Some(r) = trimmed.strip_prefix("//!") {
        r
    } else {
        trimmed.trim_start_matches('/')
    };
    rest.strip_prefix(' ').unwrap_or(rest)
}

fn extract_rust(content: &str) -> Option<DocExtraction> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    // Skip leading blank lines.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    // Collect the contiguous leading doc block.
    let doc_start = i;
    let mut doc: Vec<String> = Vec::new();
    while i < lines.len() && is_rust_doc_line(lines[i].trim()) {
        doc.push(strip_rust_doc_marker(lines[i].trim()).to_string());
        i += 1;
    }
    if i == doc_start {
        return None; // no leading doc comment
    }
    let doc_text = doc.join("\n");
    if doc_text.trim().is_empty() {
        return None;
    }
    // Body = everything except the leading doc block (preserves any later code).
    let mut body_lines: Vec<&str> = Vec::with_capacity(lines.len());
    body_lines.extend_from_slice(&lines[..doc_start]);
    body_lines.extend_from_slice(&lines[i..]);
    let body_without_doc = body_lines.join("\n");
    if body_without_doc.trim().is_empty() {
        return None; // doc with no code after it — not a usable target
    }
    Some(DocExtraction {
        doc_text,
        body_without_doc,
    })
}

/// Python: handle the two common leading-docstring shapes — a module-level
/// triple-quoted string at the top of the chunk, or a `def`/`class` header
/// immediately followed by a triple-quoted docstring.
fn extract_python(content: &str) -> Option<DocExtraction> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return None;
    }

    // A def/class header may precede the docstring; keep it in the body.
    let header_end = {
        let t = lines[i].trim_start();
        if (t.starts_with("def ") || t.starts_with("class ") || t.starts_with("async def "))
            && lines[i].trim_end().ends_with(':')
        {
            i + 1
        } else {
            i
        }
    };
    let mut j = header_end;
    while j < lines.len() && lines[j].trim().is_empty() {
        j += 1;
    }
    if j >= lines.len() {
        return None;
    }

    let opener = lines[j].trim_start();
    let quote = if opener.starts_with("\"\"\"") {
        "\"\"\""
    } else if opener.starts_with("'''") {
        "'''"
    } else {
        return None; // first statement isn't a docstring
    };

    // Find the closing triple-quote (possibly on the same line).
    let first = opener.strip_prefix(quote).unwrap_or("");
    let mut doc: Vec<String> = Vec::new();
    let mut close_line = j;
    if let Some(end) = first.find(quote) {
        // Single-line docstring.
        doc.push(first[..end].trim().to_string());
    } else {
        if !first.trim().is_empty() {
            doc.push(first.trim().to_string());
        }
        let mut k = j + 1;
        let mut found = false;
        while k < lines.len() {
            if let Some(end) = lines[k].find(quote) {
                let pre = &lines[k][..end];
                if !pre.trim().is_empty() {
                    doc.push(pre.trim().to_string());
                }
                close_line = k;
                found = true;
                break;
            }
            doc.push(lines[k].trim().to_string());
            k += 1;
        }
        if !found {
            return None; // unterminated docstring
        }
    }

    let doc_text = doc.join("\n");
    if doc_text.trim().is_empty() {
        return None;
    }

    // Body = header lines + everything after the docstring (doc removed).
    let mut body_lines: Vec<&str> = Vec::with_capacity(lines.len());
    body_lines.extend_from_slice(&lines[..header_end]);
    if close_line + 1 < lines.len() {
        body_lines.extend_from_slice(&lines[close_line + 1..]);
    }
    let body_without_doc = body_lines.join("\n");
    if body_without_doc.trim().is_empty() {
        return None;
    }
    Some(DocExtraction {
        doc_text,
        body_without_doc,
    })
}

/// Replace whole-token occurrences of any identifier in `idents` with a generic
/// placeholder (`"IDENT"`). Tokenization splits on any char that is not
/// alphanumeric or `_`, so `foo_bar` is one token and substrings are never
/// partially redacted. Used by the M3 variant on both query and chunk so the
/// metric reflects semantics rather than identifier echo.
pub fn redact_identifiers(text: &str, idents: &std::collections::HashSet<String>) -> String {
    if idents.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if !token.is_empty() {
            if idents.contains(token.as_str()) {
                out.push_str("IDENT");
            } else {
                out.push_str(token);
            }
            token.clear();
        }
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_leading_outer_doc() {
        let content = "/// Compute the cosine similarity between two vectors.\n\
                       /// Both must be L2-normalized.\n\
                       pub fn cosine(a: &[f32], b: &[f32]) -> f32 {\n\
                       \x20\x20\x20\x20dot(a, b)\n\
                       }\n";
        let e = extract_leading_docstring(content, "rust").expect("doc");
        assert_eq!(
            e.doc_text,
            "Compute the cosine similarity between two vectors.\nBoth must be L2-normalized."
        );
        assert!(e.body_without_doc.contains("pub fn cosine"));
        assert!(!e.body_without_doc.contains("cosine similarity"));
    }

    #[test]
    fn rust_inner_doc_module() {
        let content = "//! Vector math helpers.\n\
                       \n\
                       pub fn dot(a: &[f32], b: &[f32]) -> f32 { 0.0 }\n";
        let e = extract_leading_docstring(content, "rust").expect("doc");
        assert_eq!(e.doc_text, "Vector math helpers.");
        assert!(e.body_without_doc.contains("pub fn dot"));
    }

    #[test]
    fn rust_quad_slash_is_not_doc() {
        let content = "//// not a doc comment\npub fn f() {}\n";
        assert!(extract_leading_docstring(content, "rust").is_none());
    }

    #[test]
    fn rust_no_body_returns_none() {
        let content = "/// only docs, no code\n/// more docs\n";
        assert!(extract_leading_docstring(content, "rust").is_none());
    }

    #[test]
    fn rust_skips_leading_blank_lines() {
        let content = "\n\n/// Doc after blanks.\nfn f() {}\n";
        let e = extract_leading_docstring(content, "rust").expect("doc");
        assert_eq!(e.doc_text, "Doc after blanks.");
    }

    #[test]
    fn python_module_docstring() {
        let content = "\"\"\"Parse the configuration file.\"\"\"\n\
                       import os\n\
                       def load(): pass\n";
        let e = extract_leading_docstring(content, "python").expect("doc");
        assert_eq!(e.doc_text, "Parse the configuration file.");
        assert!(e.body_without_doc.contains("import os"));
        assert!(!e.body_without_doc.contains("Parse the configuration"));
    }

    #[test]
    fn python_function_docstring_multiline() {
        let content = "def authenticate(user, token):\n\
                       \x20\x20\x20\x20\"\"\"Verify a user's token.\n\
                       \n\
                       \x20\x20\x20\x20Returns True on success.\n\
                       \x20\x20\x20\x20\"\"\"\n\
                       \x20\x20\x20\x20return True\n";
        let e = extract_leading_docstring(content, "python").expect("doc");
        assert!(e.doc_text.contains("Verify a user's token."));
        assert!(e.doc_text.contains("Returns True on success."));
        // The def header stays in the body; the docstring is removed.
        assert!(e.body_without_doc.contains("def authenticate"));
        assert!(e.body_without_doc.contains("return True"));
        assert!(!e.body_without_doc.contains("Verify a user's token"));
    }

    #[test]
    fn unsupported_language_returns_none() {
        assert!(extract_leading_docstring("/// doc\nfn f(){}", "go").is_none());
    }

    #[test]
    fn first_paragraph_stops_at_blank() {
        let doc = "Short summary line.\n\nLonger detail that should be dropped.";
        assert_eq!(first_paragraph(doc), "Short summary line.");
    }

    #[test]
    fn redact_replaces_whole_tokens_only() {
        let idents: std::collections::HashSet<String> =
            ["cosine", "dot"].iter().map(|s| s.to_string()).collect();
        let redacted = redact_identifiers("fn cosine() calls dot and cosine_sim", &idents);
        // `cosine` and `dot` redacted; `cosine_sim` is a different token, kept.
        assert_eq!(redacted, "fn IDENT() calls IDENT and cosine_sim");
    }

    #[test]
    fn redact_empty_idents_is_identity() {
        let idents: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(
            redact_identifiers("anything goes", &idents),
            "anything goes"
        );
    }
}
