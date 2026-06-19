//! Metamath language backend (ADR-025).
//!
//! Regex-based (no tree-sitter grammar), over comment-stripped text (Metamath
//! uses `$( … $)` comments). Metamath is a token stream, not line-structured:
//! labelled statements `LABEL $a …` / `LABEL $p …` are axioms / provable
//! assertions, `$c …` / `$v …` declare constants / variables, `$[ file.mm $]`
//! includes a file. Shadow-ASR contract: names + kinds only.

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::regex_fv_util::{CommentStyle, strip_comments_preserving_lines};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolReference};

pub static METAMATH_BACKEND: MetamathBackend = MetamathBackend;
pub struct MetamathBackend;

struct Res {
    /// `LABEL $a` / `LABEL $p` — a labelled axiom / provable assertion.
    assertion: Regex,
    /// `$c c1 c2 … $.` constant declaration. Captures the token list.
    constants: Regex,
    /// `$[ path $]` file include.
    include: Regex,
}

fn res() -> &'static Res {
    static RE: OnceLock<Res> = OnceLock::new();
    RE.get_or_init(|| Res {
        assertion: Regex::new(r"([A-Za-z0-9_.\-]+)\s+\$([ap])\b").expect("mm assertion regex"),
        constants: Regex::new(r"\$c\s+([^$]+?)\s*\$\.").expect("mm constants regex"),
        include: Regex::new(r"\$\[\s*([^$\s]+)\s*\$\]").expect("mm include regex"),
    })
}

fn line_of(src: &str, byte_offset: usize) -> u32 {
    (src[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1) as u32
}

impl LanguageBackend for MetamathBackend {
    fn language_name(&self) -> &'static str {
        "metamath"
    }

    fn lex_config(&self) -> crate::parsing::occurrences::LexConfig {
        crate::parsing::occurrences::LexConfig::metamath_style()
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let content = strip_comments_preserving_lines(content, CommentStyle::Metamath);
        let re = res();
        let mut out = Vec::new();
        for cap in re.assertion.captures_iter(&content) {
            if let (Some(label), Some(kindm)) = (cap.get(1), cap.get(2)) {
                let start_line = line_of(&content, label.start());
                out.push(Symbol {
                    file_id: 0,
                    name: label.as_str().to_string(),
                    // $a (axiom) and $p (provable assertion / theorem) are both
                    // assertion-level declarations.
                    kind: SymbolKind::Function,
                    start_line,
                    end_line: start_line,
                    parent_id: None,
                    visibility: Some("public".into()),
                    signature: Some(format!("${}", kindm.as_str())),
                    ..Default::default()
                });
            }
        }
        for cap in re.constants.captures_iter(&content) {
            if let (Some(m), Some(toks)) = (cap.get(0), cap.get(1)) {
                let start_line = line_of(&content, m.start());
                for tok in toks.as_str().split_whitespace() {
                    out.push(Symbol {
                        file_id: 0,
                        name: tok.to_string(),
                        kind: SymbolKind::Const,
                        start_line,
                        end_line: start_line,
                        parent_id: None,
                        visibility: Some("public".into()),
                        signature: None,
                        ..Default::default()
                    });
                }
            }
        }
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let content = strip_comments_preserving_lines(content, CommentStyle::Metamath);
        let re = res();
        let mut out = Vec::new();
        for cap in re.include.captures_iter(&content) {
            if let (Some(m), Some(path)) = (cap.get(0), cap.get(1)) {
                out.push(Import {
                    target_raw: path.as_str().to_string(),
                    source_line: line_of(&content, m.start()),
                    alias: None,
                });
            }
        }
        out
    }

    fn extract_references(&self, _content: &str) -> Vec<SymbolReference> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
$( Declare the constant symbols $)
$c 0 + = -> ( ) wff |- $.
$v x y $.
$[ set.mm $]

wnew $a wff x $.
ax1 $a |- ( x -> x ) $.
$( th_fake $a should be ignored $)
th1 $p |- ( x -> x ) $= ax1 $.
"#;

    #[test]
    fn language_name() {
        assert_eq!(METAMATH_BACKEND.language_name(), "metamath");
    }

    #[test]
    fn extracts_assertions_and_constants_skips_comments() {
        let syms = METAMATH_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"ax1"), "missing ax1: {names:?}");
        assert!(names.contains(&"th1"));
        assert!(names.contains(&"wnew"));
        assert!(names.contains(&"+"), "missing constant '+': {names:?}");
        assert!(!names.contains(&"th_fake"), "comment leak: {names:?}");
    }

    #[test]
    fn extracts_includes() {
        let imports = METAMATH_BACKEND.extract_imports(SAMPLE);
        assert!(
            imports.iter().any(|i| i.target_raw == "set.mm"),
            "missing include"
        );
    }

    #[test]
    fn empty_input() {
        assert!(METAMATH_BACKEND.extract_symbols("").is_empty());
        assert!(METAMATH_BACKEND.extract_imports("").is_empty());
    }
}
