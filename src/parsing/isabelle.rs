//! Isabelle/HOL language backend (ADR-025).
//!
//! No maintained `tree-sitter-isabelle` crate exists, so extraction is
//! regex-based (like `coq.rs`), over comment-stripped text (Isabelle uses nested
//! `(* … *)` comments — see [`crate::parsing::regex_fv_util`]). Shadow-ASR
//! contract: names + kinds only; parameter/return type fields stay empty
//! (`Default::default()`), like the other FV backends.

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::regex_fv_util::{CommentStyle, strip_comments_preserving_lines};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolReference};

pub static ISABELLE_BACKEND: IsabelleBackend = IsabelleBackend;
pub struct IsabelleBackend;

struct Res {
    decl: Regex,
    theory_imports: Regex,
}

fn res() -> &'static Res {
    static RE: OnceLock<Res> = OnceLock::new();
    RE.get_or_init(|| Res {
        // `<keyword> <name>` — the canonical Isar declaration forms. The name is
        // an identifier (Isabelle allows primes and dots in some names; keep it
        // simple and accept the leading identifier token).
        decl: Regex::new(
            r#"(?m)^\s*(theorem|lemma|corollary|proposition|definition|fun|primrec|datatype|record|locale|class|instantiation|abbreviation|typedef|type_synonym|theory)\s+([A-Za-z_][A-Za-z0-9_']*)"#,
        )
        .expect("isabelle decl regex"),
        // `theory T imports A B C begin` — capture the import list.
        theory_imports: Regex::new(r"(?m)^\s*theory\s+[A-Za-z0-9_']+\s+imports\s+([^\n]+?)\s+begin")
            .expect("isabelle imports regex"),
    })
}

fn line_of(src: &str, byte_offset: usize) -> u32 {
    (src[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1) as u32
}

fn kind_for(keyword: &str) -> SymbolKind {
    match keyword {
        "theorem" | "lemma" | "corollary" | "proposition" => SymbolKind::Function,
        "definition" | "fun" | "primrec" | "abbreviation" => SymbolKind::Function,
        "datatype" => SymbolKind::Enum,
        "record" => SymbolKind::Struct,
        "class" => SymbolKind::Class,
        "locale" | "instantiation" | "theory" => SymbolKind::Module,
        _ => SymbolKind::Other, // typedef / type_synonym
    }
}

impl LanguageBackend for IsabelleBackend {
    fn language_name(&self) -> &'static str {
        "isabelle"
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

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let re = res();
        let mut out = Vec::new();
        for cap in re.theory_imports.captures_iter(&content) {
            if let (Some(m), Some(list)) = (cap.get(0), cap.get(1)) {
                let line = line_of(&content, m.start());
                for t in list.as_str().split_whitespace() {
                    let t = t.trim_matches('"');
                    if !t.is_empty() {
                        out.push(Import {
                            target_raw: t.to_string(),
                            source_line: line,
                            alias: None,
                        });
                    }
                }
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
theory Demo imports Main HOL.List begin

(* lemma fake_in_comment : True *)

definition double :: "nat ⇒ nat" where "double x = x + x"

datatype tree = Leaf | Node tree tree

record point = x :: nat  y :: nat

lemma double_eq : "double n = n + n"
  by (simp add: double_def)

end
"#;

    #[test]
    fn language_name() {
        assert_eq!(ISABELLE_BACKEND.language_name(), "isabelle");
    }

    #[test]
    fn extracts_decls_and_skips_comments() {
        let syms = ISABELLE_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"double"), "missing double: {names:?}");
        assert!(names.contains(&"tree"));
        assert!(names.contains(&"point"));
        assert!(names.contains(&"double_eq"));
        assert!(
            !names.contains(&"fake_in_comment"),
            "comment leak: {names:?}"
        );
        let kind = |n: &str| syms.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(kind("tree"), Some(SymbolKind::Enum));
        assert_eq!(kind("point"), Some(SymbolKind::Struct));
    }

    #[test]
    fn extracts_theory_imports() {
        let imports = ISABELLE_BACKEND.extract_imports(SAMPLE);
        let t: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(t.contains(&"Main"), "imports: {t:?}");
        assert!(t.contains(&"HOL.List"));
    }

    #[test]
    fn empty_input() {
        assert!(ISABELLE_BACKEND.extract_symbols("").is_empty());
        assert!(ISABELLE_BACKEND.extract_imports("").is_empty());
    }
}
