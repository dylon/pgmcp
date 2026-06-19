//! Tamarin-prover (`.spthy`) language backend (ADR-025).
//!
//! Regex-based, over comment-stripped text (Tamarin uses C-style `/* … */` block
//! and `//` line comments). Captures the security-protocol declaration forms.
//! Shadow-ASR contract: names + kinds only.

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::regex_fv_util::{CommentStyle, strip_comments_preserving_lines};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolReference};

pub static TAMARIN_BACKEND: TamarinBackend = TamarinBackend;
pub struct TamarinBackend;

fn decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*(rule|lemma|restriction|axiom|theory)\s+([A-Za-z_][A-Za-z0-9_']*)")
            .expect("tamarin decl regex")
    })
}

fn line_of(src: &str, byte_offset: usize) -> u32 {
    (src[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1) as u32
}

fn kind_for(keyword: &str) -> SymbolKind {
    match keyword {
        "rule" | "lemma" => SymbolKind::Function,
        "theory" => SymbolKind::Module,
        _ => SymbolKind::Other, // restriction / axiom
    }
}

impl LanguageBackend for TamarinBackend {
    fn language_name(&self) -> &'static str {
        "tamarin"
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CStyle);
        let mut out = Vec::new();
        for cap in decl_re().captures_iter(&content) {
            if let (Some(kw), Some(name)) = (cap.get(1), cap.get(2)) {
                let start_line = line_of(&content, kw.start());
                out.push(Symbol {
                    file_id: 0,
                    name: name.as_str().to_string(),
                    kind: kind_for(kw.as_str()),
                    start_line,
                    end_line: start_line,
                    parent_id: None,
                    visibility: Some("public".into()),
                    signature: None,
                    ..Default::default()
                });
            }
        }
        out
    }

    fn extract_imports(&self, _content: &str) -> Vec<Import> {
        Vec::new()
    }

    fn extract_references(&self, _content: &str) -> Vec<SymbolReference> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
theory SignedDH begin

builtins: diffie-hellman, signing

// rule fake_line_comment: [ ] --> [ ]
/* rule fake_block: [ ] --> [ ] */

rule Register_pk:
    [ Fr(~ltk) ] --> [ !Ltk($A, ~ltk) ]

lemma secrecy:
  "All x #i. Secret(x) @ #i ==> not (Ex #j. K(x) @ #j)"

end
"#;

    #[test]
    fn language_name() {
        assert_eq!(TAMARIN_BACKEND.language_name(), "tamarin");
    }

    #[test]
    fn extracts_decls_skips_comments() {
        let syms = TAMARIN_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"SignedDH"), "missing theory: {names:?}");
        assert!(names.contains(&"Register_pk"));
        assert!(names.contains(&"secrecy"));
        assert!(
            !names.contains(&"fake_line_comment"),
            "line-comment leak: {names:?}"
        );
        assert!(
            !names.contains(&"fake_block"),
            "block-comment leak: {names:?}"
        );
        let kind = |n: &str| syms.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(kind("Register_pk"), Some(SymbolKind::Function));
        assert_eq!(kind("SignedDH"), Some(SymbolKind::Module));
    }

    #[test]
    fn empty_input() {
        assert!(TAMARIN_BACKEND.extract_symbols("").is_empty());
    }
}
