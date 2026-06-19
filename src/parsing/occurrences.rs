//! Language-agnostic token-level occurrence extraction (ADR-024, item 10).
//!
//! Backs `LanguageBackend::extract_occurrences`. Rather than a bespoke per-grammar
//! walk for each of ~17 backends, a single lexical scanner — parameterized by a
//! small per-language [`LexConfig`] (line/block/doc comment markers + string
//! delimiters) — classifies EVERY identifier occurrence as
//! `code_reference` / `comment` / `string` / `doc` with line + column offsets.
//! This is the user's "differentiate `x` in source from `x` in commentary"
//! requirement, uniformly across every indexed language. (Definitions are marked
//! by the extraction cron, which matches occurrences against persisted
//! `file_symbols`; binder `type_tags` are attached there too.)
//!
//! Columns are 0-based UTF-8 **character** offsets within the line (the LSP
//! boundary converts to UTF-16 if a client needs it). The scanner is a single
//! pass over `char_indices`, so it is `O(n)` in source length.

#![allow(dead_code)]

use crate::parsing::occurrence_kind::OccurrenceKind;
use crate::parsing::symbols::Occurrence;

/// Per-language lexical syntax for comments and strings. Markers are matched
/// longest-first within each class so `///` (doc) beats `//` (line).
#[derive(Debug, Clone)]
pub struct LexConfig {
    /// Doc-comment line prefixes (e.g. `///`, `//!`). Checked before
    /// `line_comment` so doc beats ordinary line comments.
    pub doc_line: Vec<&'static str>,
    /// Ordinary line-comment prefixes (e.g. `//`, `#`, `;`, `--`, `%`).
    pub line_comment: Vec<&'static str>,
    /// Block-comment delimiter pairs (e.g. `("/*","*/")`, `("(*","*)")`).
    pub block_comment: Vec<(&'static str, &'static str)>,
    /// Doc block-comment pairs (e.g. `("/**","*/")`). Checked before `block_comment`.
    pub doc_block: Vec<(&'static str, &'static str)>,
    /// String-literal delimiter pairs (longest opener wins; backslash escapes).
    pub strings: Vec<(&'static str, &'static str)>,
    /// Whether `block_comment` nests (ML-family `(* (* *) *)`).
    pub nested_block: bool,
}

impl Default for LexConfig {
    fn default() -> Self {
        Self::c_style()
    }
}

impl LexConfig {
    /// C / Rust / Java / Scala / JS / TS / Go: `//`,`/* */`, `"`, `'`; doc `///`,`//!`,`/**`.
    pub fn c_style() -> Self {
        LexConfig {
            doc_line: vec!["///", "//!"],
            line_comment: vec!["//"],
            block_comment: vec![("/*", "*/")],
            doc_block: vec![("/**", "*/")],
            strings: vec![("\"", "\""), ("'", "'")],
            nested_block: false,
        }
    }

    /// Python / Ruby / shell / TOML: `#` line comments; `"""`/`'''`/`"`/`'` strings.
    pub fn hash_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec!["#"],
            block_comment: vec![],
            doc_block: vec![],
            strings: vec![
                ("\"\"\"", "\"\"\""),
                ("'''", "'''"),
                ("\"", "\""),
                ("'", "'"),
            ],
            nested_block: false,
        }
    }

    /// Lisp / Clojure: `;` line comments, `"` strings.
    pub fn lisp_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec![";"],
            block_comment: vec![],
            doc_block: vec![],
            strings: vec![("\"", "\"")],
            nested_block: false,
        }
    }

    /// ML-family (Coq, Isabelle, Why3): nested `(* *)`, `"` strings.
    pub fn ml_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec![],
            block_comment: vec![("(*", "*)")],
            doc_block: vec![],
            strings: vec![("\"", "\"")],
            nested_block: true,
        }
    }

    /// Metamath: `$( $)` comments, no string class.
    pub fn metamath_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec![],
            block_comment: vec![("$(", "$)")],
            doc_block: vec![],
            strings: vec![],
            nested_block: false,
        }
    }

    /// Lean 4: `--` line, nested `/- -/` block, `"` strings.
    pub fn lean_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec!["--"],
            block_comment: vec![("/-", "-/")],
            doc_block: vec![("/--", "-/")],
            strings: vec![("\"", "\"")],
            nested_block: true,
        }
    }

    /// TLA⁺: `\*` line, nested `(* *)` block, `"` strings.
    pub fn tla_style() -> Self {
        LexConfig {
            doc_line: vec![],
            line_comment: vec!["\\*"],
            block_comment: vec![("(*", "*)")],
            doc_block: vec![],
            strings: vec![("\"", "\"")],
            nested_block: true,
        }
    }
}

/// Cross-language keyword stoplist — the highest-frequency reserved words across
/// the indexed languages. Skipped in CODE so the occurrence index is not
/// dominated by `let`/`fn`/`if` (which no one looks up), keeping it
/// lookup-shaped without per-language keyword tables. Kept inside comments and
/// strings (a keyword-shaped word in prose may be searched).
const SKIP_KEYWORDS: &[&str] = &[
    "let",
    "fn",
    "def",
    "fun",
    "val",
    "var",
    "const",
    "if",
    "else",
    "elif",
    "for",
    "while",
    "loop",
    "do",
    "return",
    "yield",
    "break",
    "continue",
    "match",
    "case",
    "switch",
    "class",
    "struct",
    "enum",
    "trait",
    "impl",
    "interface",
    "type",
    "module",
    "mod",
    "use",
    "import",
    "from",
    "as",
    "pub",
    "public",
    "private",
    "protected",
    "static",
    "final",
    "true",
    "false",
    "null",
    "none",
    "nil",
    "self",
    "this",
    "super",
    "new",
    "in",
    "is",
    "and",
    "or",
    "not",
    "where",
    "with",
    "begin",
    "end",
    "then",
    "of",
    "to",
];

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Internal scanner state.
enum St {
    Normal,
    /// Line comment / doc-line: ends at newline. Carries its occurrence kind.
    Line(OccurrenceKind),
    /// Block comment: (opener, closer, kind, nest-depth).
    Block(&'static str, &'static str, OccurrenceKind, usize),
    /// String literal: closer.
    Str(&'static str),
}

fn match_marker(rest: &str, markers: &[&'static str]) -> Option<&'static str> {
    markers
        .iter()
        .filter(|m| rest.starts_with(**m))
        .max_by_key(|m| m.len())
        .copied()
}
fn match_pair(
    rest: &str,
    pairs: &[(&'static str, &'static str)],
) -> Option<(&'static str, &'static str)> {
    pairs
        .iter()
        .filter(|(o, _)| rest.starts_with(*o))
        .max_by_key(|(o, _)| o.len())
        .copied()
}

/// One in-progress identifier.
#[derive(Default)]
struct Pending {
    text: String,
    line: u32,
    col: u32,
}

/// Extract every identifier occurrence from `content`, classified per `cfg`.
pub fn extract_occurrences_textual(content: &str, cfg: &LexConfig) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut state = St::Normal;
    let mut line: u32 = 1;
    let mut col: u32 = 0;
    let mut pend = Pending::default();

    let chars: Vec<(usize, char)> = content.char_indices().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let (byte_off, c) = chars[i];
        let rest = &content[byte_off..];

        // ── 1. State transitions ──────────────────────────────────────────
        match &mut state {
            St::Normal => {
                if let Some(m) = match_marker(rest, &cfg.doc_line) {
                    flush(&mut out, &mut pend, OccurrenceKind::CodeReference);
                    state = St::Line(OccurrenceKind::Doc);
                    advance(&mut i, &mut line, &mut col, m);
                    continue;
                }
                if let Some(m) = match_marker(rest, &cfg.line_comment) {
                    flush(&mut out, &mut pend, OccurrenceKind::CodeReference);
                    state = St::Line(OccurrenceKind::Comment);
                    advance(&mut i, &mut line, &mut col, m);
                    continue;
                }
                if let Some((o, close)) = match_pair(rest, &cfg.doc_block) {
                    flush(&mut out, &mut pend, OccurrenceKind::CodeReference);
                    state = St::Block(o, close, OccurrenceKind::Doc, 1);
                    advance(&mut i, &mut line, &mut col, o);
                    continue;
                }
                if let Some((o, close)) = match_pair(rest, &cfg.block_comment) {
                    flush(&mut out, &mut pend, OccurrenceKind::CodeReference);
                    state = St::Block(o, close, OccurrenceKind::Comment, 1);
                    advance(&mut i, &mut line, &mut col, o);
                    continue;
                }
                if let Some((o, close)) = match_pair(rest, &cfg.strings) {
                    flush(&mut out, &mut pend, OccurrenceKind::CodeReference);
                    state = St::Str(close);
                    advance(&mut i, &mut line, &mut col, o);
                    continue;
                }
            }
            St::Line(k) => {
                if c == '\n' {
                    flush(&mut out, &mut pend, *k);
                    state = St::Normal;
                    step(&mut i, &mut line, &mut col, c);
                    continue;
                }
            }
            St::Block(opener, closer, k, depth) => {
                let (opener, closer, k) = (*opener, *closer, *k);
                if cfg.nested_block && rest.starts_with(opener) {
                    flush(&mut out, &mut pend, k);
                    *depth += 1;
                    advance(&mut i, &mut line, &mut col, opener);
                    continue;
                }
                if rest.starts_with(closer) {
                    flush(&mut out, &mut pend, k);
                    *depth -= 1;
                    let done = *depth == 0;
                    advance(&mut i, &mut line, &mut col, closer);
                    if done {
                        state = St::Normal;
                    }
                    continue;
                }
            }
            St::Str(closer) => {
                let closer = *closer;
                if c == '\\' {
                    flush(&mut out, &mut pend, OccurrenceKind::String);
                    step(&mut i, &mut line, &mut col, c);
                    if i < chars.len() {
                        let (_, e) = chars[i];
                        step(&mut i, &mut line, &mut col, e);
                    }
                    continue;
                }
                if rest.starts_with(closer) {
                    flush(&mut out, &mut pend, OccurrenceKind::String);
                    advance(&mut i, &mut line, &mut col, closer);
                    state = St::Normal;
                    continue;
                }
            }
        }

        // ── 2. Identifier accumulation (uniform flush-on-boundary) ─────────
        let kind = match &state {
            St::Normal => OccurrenceKind::CodeReference,
            St::Line(k) => *k,
            St::Block(_, _, k, _) => *k,
            St::Str(_) => OccurrenceKind::String,
        };
        if pend.text.is_empty() {
            if is_ident_start(c) {
                pend.text.push(c);
                pend.line = line;
                pend.col = col;
            }
        } else if is_ident_continue(c) {
            pend.text.push(c);
        } else {
            flush(&mut out, &mut pend, kind);
            if is_ident_start(c) {
                pend.text.push(c);
                pend.line = line;
                pend.col = col;
            }
        }

        step(&mut i, &mut line, &mut col, c);
    }

    let kind = match &state {
        St::Normal => OccurrenceKind::CodeReference,
        St::Line(k) => *k,
        St::Block(_, _, k, _) => *k,
        St::Str(_) => OccurrenceKind::String,
    };
    flush(&mut out, &mut pend, kind);
    out
}

/// Emit the pending identifier (if any) as an occurrence of `kind`, then reset.
fn flush(out: &mut Vec<Occurrence>, pend: &mut Pending, kind: OccurrenceKind) {
    if pend.text.is_empty() {
        return;
    }
    let skip = matches!(kind, OccurrenceKind::CodeReference)
        && SKIP_KEYWORDS.contains(&pend.text.to_ascii_lowercase().as_str());
    if !skip {
        let len = pend.text.chars().count() as u32;
        out.push(Occurrence {
            name: std::mem::take(&mut pend.text),
            start_line: pend.line,
            start_col: pend.col,
            end_col: pend.col + len,
            occurrence_kind: kind,
            type_tags: Vec::new(),
        });
    } else {
        pend.text.clear();
    }
}

/// Advance the cursor past a multi-char marker, updating line/col.
fn advance(i: &mut usize, line: &mut u32, col: &mut u32, marker: &str) {
    for c in marker.chars() {
        step(i, line, col, c);
    }
}

/// Advance one char, updating line/col.
fn step(i: &mut usize, line: &mut u32, col: &mut u32, c: char) {
    *i += 1;
    if c == '\n' {
        *line += 1;
        *col = 0;
    } else {
        *col += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_kinds(content: &str, cfg: &LexConfig) -> Vec<(String, OccurrenceKind)> {
        extract_occurrences_textual(content, cfg)
            .into_iter()
            .map(|o| (o.name, o.occurrence_kind))
            .collect()
    }

    #[test]
    fn classifies_code_comment_string_doc() {
        let src = "/// docword\nlet alpha = beta; // commentword\nlet s = \"stringword\";";
        let got = names_kinds(src, &LexConfig::c_style());
        let has =
            |n: &str, k: OccurrenceKind| got.iter().any(|(name, kind)| name == n && *kind == k);
        assert!(has("docword", OccurrenceKind::Doc), "{got:?}");
        assert!(has("alpha", OccurrenceKind::CodeReference), "{got:?}");
        assert!(has("beta", OccurrenceKind::CodeReference), "{got:?}");
        assert!(has("commentword", OccurrenceKind::Comment), "{got:?}");
        assert!(has("stringword", OccurrenceKind::String), "{got:?}");
        assert!(
            !got.iter()
                .any(|(n, k)| n == "let" && *k == OccurrenceKind::CodeReference),
            "keyword `let` must be skipped in code"
        );
    }

    #[test]
    fn multiword_comment_words_are_separate() {
        let src = "// alpha beta gamma";
        let got: Vec<String> = extract_occurrences_textual(src, &LexConfig::c_style())
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert_eq!(got, vec!["alpha", "beta", "gamma"], "{got:?}");
    }

    #[test]
    fn column_offsets_are_zero_based_chars() {
        let src = "  alpha";
        let occ = extract_occurrences_textual(src, &LexConfig::c_style());
        let a = occ.iter().find(|o| o.name == "alpha").expect("alpha");
        assert_eq!((a.start_line, a.start_col, a.end_col), (1, 2, 7));
    }

    #[test]
    fn ml_nested_block_comment() {
        let src = "Definition d := 0. (* outer alpha (* inner beta *) gamma *) Theorem t.";
        let got = names_kinds(src, &LexConfig::ml_style());
        for w in ["outer", "alpha", "inner", "beta", "gamma"] {
            assert!(
                got.iter()
                    .any(|(n, k)| n == w && *k == OccurrenceKind::Comment),
                "{w} should be Comment: {got:?}"
            );
        }
        assert!(
            got.iter()
                .any(|(n, k)| n == "t" && *k == OccurrenceKind::CodeReference),
            "code after the comment must be code: {got:?}"
        );
    }

    #[test]
    fn hash_style_python() {
        let src = "x = func(y)  # noteword\ns = \"strword\"";
        let got = names_kinds(src, &LexConfig::hash_style());
        assert!(
            got.iter()
                .any(|(n, k)| n == "func" && *k == OccurrenceKind::CodeReference),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(n, k)| n == "noteword" && *k == OccurrenceKind::Comment),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(n, k)| n == "strword" && *k == OccurrenceKind::String),
            "{got:?}"
        );
    }

    #[test]
    fn empty_is_empty() {
        assert!(extract_occurrences_textual("", &LexConfig::c_style()).is_empty());
    }
}
