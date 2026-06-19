# ADR-025: Formal-verification language backends + comment-strip utility

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-013 (disk-fallback symbol extraction). Files:
  `src/parsing/{isabelle,metamath,why3,tamarin,regex_fv_util}.rs`; `src/parsing/coq.rs`.

## Context

pgmcp indexes several formal-verification (FV) languages, but only Coq/Rocq, TLA⁺, and Lean
had symbol backends. Isabelle/HOL, Metamath, Why3, and Tamarin-prover files were indexed for
content/embeddings but produced **zero symbols** — no definitions, no graph nodes, invisible
to `lsp_query`, `workspace_symbol`, architecture analysis, etc. FV is a first-class part of
this user's workspace, so the gap matters.

Separately, the Coq backend carried a documented **comment-leak bug**: its line-anchored
declaration regexes (`(?m)^\s*Theorem\s+(name)`) match a keyword inside a `(* … *)` comment,
emitting phantom symbols.

## Decision

### Four new regex backends

No maintained `tree-sitter-{isabelle,metamath,why3,tamarin}` crate exists, so each is a
regex backend modeled on `coq.rs`, wired the standard way (`registry.rs` →
`BACKEND_LANGUAGES` → `registry_dispatches_landed_backends` test → per-backend unit tests):

| Backend | Declaration forms captured | Imports | Comments |
|---|---|---|---|
| Isabelle/HOL | `theorem/lemma/definition/fun/datatype/record/locale/class/theory/…` | `theory T imports …` | `(* … *)` nested |
| Metamath | `LABEL $a`/`$p` assertions, `$c` constants | `$[ file $]` | `$( … $)` |
| Why3 | `let/val/predicate/function/type/inductive/lemma/theory/module/…` | `use`/`clone` | `(* … *)` nested |
| Tamarin | `rule/lemma/restriction/theory` | — | `/* */` + `//` |

Each maps its declaration keyword to the closest `SymbolKind` (theorem/lemma → `Function`,
datatype → `Enum`, record → `Struct`, theory/locale → `Module`, …). Shadow-ASR contract
(per the Coq precedent): names + kinds only; structured parameter/return type fields stay at
`Default::default()` — these languages can't yield meaningful type tags without real
inference, and downstream tools LEFT-JOIN + COALESCE so the empty shape degrades gracefully.

### Shared comment-strip utility (Boy-Scout fix for Coq)

`src/parsing/regex_fv_util.rs::strip_comments_preserving_lines` blanks comment spans —
replacing every non-newline byte with a space — **before** the declaration regexes run,
while **preserving byte offsets and line numbers** (so `line_of` stays correct and captured
names outside comments are byte-identical). It handles nested `(* *)` (Coq/Isabelle/Why3),
`$( $)` (Metamath), and C-style `/* */` + `//` (Tamarin) via a `CommentStyle` enum. The Coq
backend now calls it (fixing the documented leak — regression test:
`comment_keywords_are_not_extracted`); the four new backends use it too.

The same per-language comment knowledge feeds ADR-024's occurrence scanner via `LexConfig`
(distinct types: the FV util strips comments for *symbol* regexes; `LexConfig` *classifies*
comment vs code for the *occurrence* index).

### Lean

The Lean coverage collapse was the disk-fallback data path, fixed in ADR-013's general
duplicate-pointer repair (Phase A2 of this program) — not a backend depth problem — so Lean
keeps its existing tree-sitter backend.

## Consequences

- Isabelle / Metamath / Why3 / Tamarin symbols now populate `file_symbols`, the unified
  graph, `lsp_query`, and architecture analysis.
- The Coq phantom-symbol leak is closed, with a non-vacuous regression test.
- Byte-offset-preserving comment stripping is reusable for any future regex backend.
- Tested: per-backend unit tests (declaration capture + comment-leak negative cases) +
  the comment-strip util's own tests (offset preservation, nesting, UTF-8 safety).
