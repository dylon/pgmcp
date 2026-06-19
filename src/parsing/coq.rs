//! Coq / Rocq language backend.
//!
//! Shadow-ASR contract: Coq's regex-only extraction yields names + kinds
//! but no structured parameter types or return-type expressions. Symbols
//! emitted from this backend leave the shadow-ASR fields (`parameters`,
//! `return_type`, `generic_params`, `effects`, `type_tags`) at their
//! `Default::default()` values — empty lists / `None`. This is the
//! intentional design contract per the plan
//! (`~/.claude/plans/would-translating-the-asts-cosmic-quill.md` § Phase
//! C — "Coq/TLA+/Lean cannot produce meaningful parameter type tags
//! without real inference; they populate type_raw and leave type_tags =
//! '{}'"). Downstream tools JOIN the shadow-ASR tables with LEFT JOIN
//! + COALESCE so Coq rows degrade gracefully into the empty-shape case.
//!
//! There is no published `tree-sitter-coq` crate on crates.io as of
//! 2026-05-20, so this backend extracts symbols, imports, and references
//! via regex patterns instead of an AST. The patterns are tight enough for
//! the canonical declaration forms (`Theorem`, `Lemma`, `Definition`,
//! `Fixpoint`, `Inductive`, `Record`, `Class`, `Instance`, `Module`,
//! `Section`, `Notation`, `Variable`, `Hypothesis`, `Axiom`) and the two
//! import shapes (`Require Import …` and `From … Require Import …`).
//! Switch to a tree-sitter grammar via local-path dep when a maintained
//! one becomes available.

#![allow(dead_code)]

use std::sync::OnceLock;

use regex::Regex;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::regex_fv_util::{CommentStyle, strip_comments_preserving_lines};
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static COQ_BACKEND: CoqBackend = CoqBackend;
pub struct CoqBackend;

/// One regex per symbol-declaration kind. Each captures the name in group 1.
struct CoqRegexes {
    /// `Theorem <name> …`, plus Lemma/Corollary/Proposition/Remark/Fact/Property.
    theorem_re: Regex,
    /// `Definition <name>`, `Fixpoint <name>`, `CoFixpoint <name>`.
    definition_re: Regex,
    /// `Inductive <name>`, `CoInductive <name>`.
    inductive_re: Regex,
    /// `Record <name>`.
    record_re: Regex,
    /// `Class <name>`.
    class_re: Regex,
    /// `Instance <name>`.
    instance_re: Regex,
    /// `Module <name>` / `Module Type <name>`. Skips `Module Import` (no decl).
    module_re: Regex,
    /// `Section <name>`.
    section_re: Regex,
    /// `Notation "<symbol>"` — first quoted string is the name.
    notation_re: Regex,
    /// `Variable <name>` / `Variables <names>` / `Hypothesis <name>` /
    /// `Hypotheses <names>` / `Axiom <name>` / `Axioms <names>`.
    declared_re: Regex,
    /// `Require Import <Mod>` / `Require Export <Mod>`. Captures comma-separated list.
    require_re: Regex,
    /// `From <Lib> Require Import <Mod>` / `Require Export <Mod>`.
    from_require_re: Regex,
    /// Inside proofs: `apply <Lemma>`, `rewrite <Lemma>`, `exact <Term>`.
    tactic_ref_re: Regex,
}

fn coq_regexes() -> &'static CoqRegexes {
    static RE: OnceLock<CoqRegexes> = OnceLock::new();
    RE.get_or_init(|| CoqRegexes {
        theorem_re: Regex::new(
            r"(?m)^\s*(?:Theorem|Lemma|Corollary|Proposition|Remark|Fact|Property)\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("theorem regex"),
        definition_re: Regex::new(
            r"(?m)^\s*(?:Definition|Fixpoint|CoFixpoint|Let|Example)\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("definition regex"),
        inductive_re: Regex::new(
            r"(?m)^\s*(?:Inductive|CoInductive|Variant)\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("inductive regex"),
        record_re: Regex::new(r"(?m)^\s*Record\s+([A-Za-z_][A-Za-z0-9_']*)").expect("record regex"),
        class_re: Regex::new(r"(?m)^\s*Class\s+([A-Za-z_][A-Za-z0-9_']*)").expect("class regex"),
        instance_re: Regex::new(
            r"(?m)^\s*(?:Global\s+|Local\s+|#\[[^\]]*\]\s*)?Instance\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("instance regex"),
        // `Module Type Foo` is captured as `Foo` via group 1 (post-skip-of-Type).
        // `Module Import Foo` / `Module Export Foo` are filtered out at use
        // site (the regex crate does not support look-around).
        module_re: Regex::new(
            r"(?m)^\s*Module\s+(?:Type\s+)?([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("module regex"),
        section_re: Regex::new(r"(?m)^\s*Section\s+([A-Za-z_][A-Za-z0-9_']*)").expect("section regex"),
        notation_re: Regex::new(r#"(?m)^\s*(?:Local\s+|Global\s+|Reserved\s+)?Notation\s+"([^"]+)""#)
            .expect("notation regex"),
        declared_re: Regex::new(
            r"(?m)^\s*(?:Variable|Variables|Hypothesis|Hypotheses|Axiom|Axioms|Conjecture|Parameter|Parameters)\s+(.+?)\s*(?::|;|\.)",
        )
        .expect("declared regex"),
        require_re: Regex::new(
            r"(?m)^\s*Require\s+(?:Import|Export)\s+([^.;\n]+)",
        )
        .expect("require regex"),
        from_require_re: Regex::new(
            r"(?m)^\s*From\s+([A-Za-z_][A-Za-z0-9_'.]*)\s+Require\s+(?:Import|Export)\s+([^.;\n]+)",
        )
        .expect("from-require regex"),
        tactic_ref_re: Regex::new(
            r"(?m)\b(?:apply|rewrite|exact|destruct|induction|specialize|pose\s+proof|generalize)\s+([A-Za-z_][A-Za-z0-9_']*)",
        )
        .expect("tactic regex"),
    })
}

/// Line number (1-based) for a byte offset in source text.
fn line_of(src: &str, byte_offset: usize) -> u32 {
    (src[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1) as u32
}

/// End-line of a Coq declaration: scan forward to the next `Qed.`, `Defined.`,
/// `Admitted.`, `End <Name>.`, or `Proof using.`-terminated proof block. For
/// non-proof declarations the body ends at the first `.` at end of line. Best-
/// effort — falls back to the start line when no terminator is found.
fn end_line_of(src: &str, start_byte: usize) -> u32 {
    let terminators = ["Qed.", "Defined.", "Admitted.", "Abort.", "Save."];
    let tail = &src[start_byte..];
    let mut best: Option<usize> = None;
    for term in &terminators {
        if let Some(idx) = tail.find(term) {
            let end = start_byte + idx + term.len();
            best = Some(best.map_or(end, |b| b.min(end)));
        }
    }
    // Fallback: first `.` at EOL.
    if best.is_none()
        && let Some(eol_idx) = tail.find(".\n").or_else(|| tail.find(".\r\n"))
    {
        best = Some(start_byte + eol_idx + 1);
    }
    let end_byte = best.unwrap_or(start_byte);
    line_of(src, end_byte.min(src.len().saturating_sub(1)))
}

impl LanguageBackend for CoqBackend {
    fn language_name(&self) -> &'static str {
        "coq"
    }

    fn lex_config(&self) -> crate::parsing::occurrences::LexConfig {
        crate::parsing::occurrences::LexConfig::ml_style()
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        // Blank `(* … *)` comments first so a keyword inside a comment cannot
        // produce a phantom symbol (offsets preserved → line numbers stay right).
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let content = content.as_str();
        let re = coq_regexes();
        let mut out: Vec<Symbol> = Vec::new();
        let push = |out: &mut Vec<Symbol>, name: String, kind: SymbolKind, start_byte: usize| {
            let start_line = line_of(content, start_byte);
            let end_line = end_line_of(content, start_byte).max(start_line);
            out.push(Symbol {
                file_id: 0,
                name,
                kind,
                start_line,
                end_line,
                parent_id: None,
                visibility: Some("public".into()),
                signature: None,
                ..Default::default()
            });
        };

        for cap in re.theorem_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Function,
                    m.start(),
                );
            }
        }
        for cap in re.definition_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Function,
                    m.start(),
                );
            }
        }
        for cap in re.inductive_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Enum,
                    m.start(),
                );
            }
        }
        for cap in re.record_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Struct,
                    m.start(),
                );
            }
        }
        for cap in re.class_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Class,
                    m.start(),
                );
            }
        }
        for cap in re.instance_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Const,
                    m.start(),
                );
            }
        }
        for cap in re.module_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                // Filter out `Module Import …` / `Module Export …` — those are
                // import qualifiers, not module declarations.
                let captured = name.as_str();
                if captured == "Import" || captured == "Export" {
                    continue;
                }
                push(
                    &mut out,
                    captured.to_string(),
                    SymbolKind::Module,
                    m.start(),
                );
            }
        }
        for cap in re.section_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Module,
                    m.start(),
                );
            }
        }
        for cap in re.notation_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                push(
                    &mut out,
                    name.as_str().to_string(),
                    SymbolKind::Other,
                    m.start(),
                );
            }
        }
        for cap in re.declared_re.captures_iter(content) {
            if let (Some(m), Some(names_blob)) = (cap.get(0), cap.get(1)) {
                // `Variables x y z : nat` declares three names. Split by whitespace.
                for tok in names_blob.as_str().split_whitespace() {
                    let tok = tok.trim_matches(|c: char| c == ',' || c == '(' || c == ')');
                    if tok.is_empty()
                        || !tok
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic() || c == '_')
                            .unwrap_or(false)
                    {
                        continue;
                    }
                    // Skip type annotations / Coq keywords appearing after the leading
                    // identifier sequence — break at the first non-identifier-looking token.
                    if !tok
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '\'')
                    {
                        break;
                    }
                    push(&mut out, tok.to_string(), SymbolKind::Const, m.start());
                }
            }
        }
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let content = content.as_str();
        let re = coq_regexes();
        let mut out: Vec<Import> = Vec::new();
        // `From Lib Require Import M1 M2 …`
        for cap in re.from_require_re.captures_iter(content) {
            if let (Some(m), Some(lib), Some(mods)) = (cap.get(0), cap.get(1), cap.get(2)) {
                let line = line_of(content, m.start());
                for module in mods.as_str().split(|c: char| c.is_whitespace() || c == ',') {
                    let module = module.trim();
                    if module.is_empty() {
                        continue;
                    }
                    out.push(Import {
                        target_raw: format!("{}.{}", lib.as_str(), module),
                        source_line: line,
                        alias: None,
                    });
                }
            }
        }
        // `Require Import M1 M2 …`
        for cap in re.require_re.captures_iter(content) {
            // Skip if this match is also inside a `From … Require …` (already handled above).
            if let (Some(m), Some(mods)) = (cap.get(0), cap.get(1)) {
                // Look backward up to 80 bytes for `From <Lib>` on the same line.
                let line_start = content[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let prefix = &content[line_start..m.start()];
                if prefix.trim_start().starts_with("From ") {
                    continue;
                }
                let line = line_of(content, m.start());
                for module in mods.as_str().split(|c: char| c.is_whitespace() || c == ',') {
                    let module = module.trim();
                    if module.is_empty() {
                        continue;
                    }
                    out.push(Import {
                        target_raw: module.to_string(),
                        source_line: line,
                        alias: None,
                    });
                }
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let content = strip_comments_preserving_lines(content, CommentStyle::CoqBlock);
        let content = content.as_str();
        let re = coq_regexes();
        let mut out: Vec<SymbolReference> = Vec::new();
        for cap in re.tactic_ref_re.captures_iter(content) {
            if let (Some(m), Some(name)) = (cap.get(0), cap.get(1)) {
                let line = line_of(content, m.start());
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw: name.as_str().to_string(),
                    ref_kind: SymbolRefKind::Call,
                    source_line: line,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
From Coq Require Import Arith List.
Require Import Lia.

Section MathExamples.

Variable n : nat.
Hypothesis n_positive : 0 < n.

Definition double (x : nat) : nat := x + x.

Fixpoint sum (l : list nat) : nat :=
  match l with
  | nil => 0
  | x :: xs => x + sum xs
  end.

Inductive tree : Type :=
  | Leaf : tree
  | Node : tree -> tree -> tree.

Record Point := { x : nat; y : nat }.

Class Eq (A : Type) := { eqb : A -> A -> bool }.

Instance NatEq : Eq nat := { eqb := Nat.eqb }.

Notation "a +' b" := (a + b) (at level 50).

Theorem double_plus : forall n : nat, double n = n + n.
Proof.
  intros. unfold double. apply Plus.plus_comm. lia.
Qed.

Lemma sum_nil : sum nil = 0.
Proof. reflexivity. Qed.

End MathExamples.
"#;

    #[test]
    fn coq_language_name() {
        assert_eq!(COQ_BACKEND.language_name(), "coq");
    }

    #[test]
    fn extract_symbols_finds_theorems_and_definitions() {
        let syms = COQ_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"double"), "missing double: {:?}", names);
        assert!(names.contains(&"sum"));
        assert!(names.contains(&"double_plus"));
        assert!(names.contains(&"sum_nil"));
    }

    #[test]
    fn extract_symbols_finds_inductives_records_classes_instances() {
        let syms = COQ_BACKEND.extract_symbols(SAMPLE);
        let by_name = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(by_name("tree").map(|s| s.kind), Some(SymbolKind::Enum));
        assert_eq!(by_name("Point").map(|s| s.kind), Some(SymbolKind::Struct));
        assert_eq!(by_name("Eq").map(|s| s.kind), Some(SymbolKind::Class));
        assert_eq!(by_name("NatEq").map(|s| s.kind), Some(SymbolKind::Const));
    }

    #[test]
    fn extract_symbols_finds_section_and_module() {
        let syms = COQ_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"MathExamples"),
            "missing section: {:?}",
            names
        );
    }

    #[test]
    fn extract_symbols_finds_variables_and_hypotheses() {
        let syms = COQ_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"n"));
        assert!(names.contains(&"n_positive"));
    }

    #[test]
    fn extract_imports_handles_from_require() {
        let imports = COQ_BACKEND.extract_imports(SAMPLE);
        let targets: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(
            targets.contains(&"Coq.Arith"),
            "missing Coq.Arith: {:?}",
            targets
        );
        assert!(targets.contains(&"Coq.List"));
        assert!(targets.contains(&"Lia"));
    }

    #[test]
    fn extract_references_finds_tactics() {
        let refs = COQ_BACKEND.extract_references(SAMPLE);
        let targets: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        // `apply Plus.plus_comm` matches `apply` + first identifier `Plus`.
        assert!(targets.iter().any(|t| *t == "Plus" || *t == "plus_comm"));
    }

    #[test]
    fn empty_input_yields_empty_vecs() {
        assert!(COQ_BACKEND.extract_symbols("").is_empty());
        assert!(COQ_BACKEND.extract_imports("").is_empty());
        assert!(COQ_BACKEND.extract_references("").is_empty());
    }

    #[test]
    fn module_import_is_not_a_declaration() {
        let src = "Module Import M.\nDefinition x := 1.\nEnd M.";
        let syms = COQ_BACKEND.extract_symbols(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        // `Module Import M` should NOT capture `Import` or `M` as a Module symbol —
        // the import qualifier is filtered out via the (?!Import\b|Export\b) lookahead.
        assert!(!names.contains(&"Import"));
        assert!(
            names.contains(&"x"),
            "x should still be extracted: {:?}",
            names
        );
    }

    #[test]
    fn comment_keywords_are_not_extracted() {
        // Regression (ADR-025): a declaration keyword inside a `(* … *)` comment
        // must NOT produce a phantom symbol now that the backend strips comments.
        let src =
            "(* Theorem fake_thm : True. Definition fake_def := 0. *)\nTheorem real_thm : True.";
        let names: Vec<String> = COQ_BACKEND
            .extract_symbols(src)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            names.iter().any(|n| n == "real_thm"),
            "real decl missing: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "fake_thm"),
            "comment leak (fake_thm): {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "fake_def"),
            "comment leak (fake_def): {names:?}"
        );
    }
}
