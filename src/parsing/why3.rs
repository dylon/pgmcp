//! Why3 language backend (ADR-025).
//!
//! Regex-based (no tree-sitter grammar), over comment-stripped text (Why3 uses
//! nested `(* … *)` comments). Captures the canonical WhyML declaration forms
//! and `use` / `clone` dependencies. Shadow-ASR contract: names + kinds only.

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::regex_fv_util::{CommentStyle, strip_comments_preserving_lines};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolReference};

pub static WHY3_BACKEND: Why3Backend = Why3Backend;
pub struct Why3Backend;

struct Res {
    decl: Regex,
    uses: Regex,
}

fn res() -> &'static Res {
    static RE: OnceLock<Res> = OnceLock::new();
    RE.get_or_init(|| Res {
        // `let rec` before `let`; name in group 2.
        decl: Regex::new(
            r"(?m)^\s*(let\s+rec|let|val|predicate|function|type|inductive|lemma|goal|axiom|theory|module|scope)\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("why3 decl regex"),
        uses: Regex::new(
            r"(?m)^\s*(?:use|clone)\s+(?:import\s+|export\s+)?([A-Za-z0-9_.']+)",
        )
        .expect("why3 use regex"),
    })
}

fn line_of(src: &str, byte_offset: usize) -> u32 {
    (src[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1) as u32
}

fn kind_for(keyword: &str) -> SymbolKind {
    match keyword {
        "type" => SymbolKind::Struct,
        "inductive" => SymbolKind::Enum,
        "theory" | "module" | "scope" => SymbolKind::Module,
        _ => SymbolKind::Function, // let / let rec / val / predicate / function / lemma / goal / axiom
    }
}

impl LanguageBackend for Why3Backend {
    fn language_name(&self) -> &'static str {
        "why3"
    }

    fn lex_config(&self) -> crate::parsing::occurrences::LexConfig {
        crate::parsing::occurrences::LexConfig::ml_style()
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let re = res();
        let mut out = Vec::new();
        for cap in re.decl.captures_iter(&content) {
            if let (Some(kw), Some(name)) = (cap.get(1), cap.get(2)) {
                let start_line = line_of(&content, kw.start());
                let keyword = kw.as_str().split_whitespace().next().unwrap_or(kw.as_str());
                // For `type`, refine via the RHS on the same line: a `|` is a
                // sum type (Enum), a `{` is a record (Struct); an alias stays
                // Struct. The regex captures only the name, so peek at the line.
                let mut kind = kind_for(keyword);
                if keyword == "type" {
                    let line_end = content[name.end()..]
                        .find('\n')
                        .map_or(content.len(), |i| name.end() + i);
                    let rhs = &content[name.end()..line_end];
                    if rhs.contains('|') {
                        kind = SymbolKind::Enum;
                    } else if rhs.contains('{') {
                        kind = SymbolKind::Struct;
                    }
                }
                out.push(Symbol {
                    file_id: 0,
                    name: name.as_str().to_string(),
                    kind,
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

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let re = res();
        let mut out = Vec::new();
        for cap in re.uses.captures_iter(&content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                out.push(Import {
                    target_raw: name.as_str().to_string(),
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
module M
  use int.Int
  use import list.List

  (* let fake_in_comment () = () *)

  type tree = Leaf | Node tree tree

  let rec sum (l : list int) : int = 0

  predicate sorted (l : list int)

  lemma sum_nonneg : forall l. sum l >= 0
end
"#;

    #[test]
    fn language_name() {
        assert_eq!(WHY3_BACKEND.language_name(), "why3");
    }

    #[test]
    fn extracts_decls_skips_comments() {
        let syms = WHY3_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"M"), "missing module: {names:?}");
        assert!(names.contains(&"tree"));
        assert!(names.contains(&"sum"));
        assert!(names.contains(&"sorted"));
        assert!(names.contains(&"sum_nonneg"));
        assert!(
            !names.contains(&"fake_in_comment"),
            "comment leak: {names:?}"
        );
        let kind = |n: &str| syms.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(kind("tree"), Some(SymbolKind::Enum));
        assert_eq!(kind("M"), Some(SymbolKind::Module));
    }

    #[test]
    fn extracts_uses() {
        let imports = WHY3_BACKEND.extract_imports(SAMPLE);
        let t: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(t.contains(&"int.Int"), "uses: {t:?}");
        assert!(t.contains(&"list.List"));
    }

    #[test]
    fn empty_input() {
        assert!(WHY3_BACKEND.extract_symbols("").is_empty());
    }
}
