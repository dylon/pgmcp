# ADR-024: Token-level symbol occurrences

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-013 (disk-fallback symbol extraction), ADR-026 (lsp_query). Migration:
  v45 (`symbol_occurrences`). Vocab: `src/parsing/occurrence_kind.rs`.

## Context

`file_symbols` records **definitions** (one row per declared symbol) with line ranges, and
`symbol_references` records resolved call/type edges. Neither answers:

1. *Every* mention of an identifier with **column offsets** (the prerequisite for LSP
   go-to / find-references / document-highlight — `symbol_references` has no columns).
2. The user's "differentiate `x` in **source** from `x` in **commentary**" — i.e. is an
   occurrence in code, a comment, a doc comment, or a string literal?
3. Lexically-scoped occurrence queries ("uses of `x` *within* function `f`").

The user asked for **all identifiers** (token-level fidelity), not a bounded subset.

## Decision

A new table **`symbol_occurrences`** (v45): `(id, file_id, name, start_line, start_col,
end_col, occurrence_kind, enclosing_symbol_id, resolved_target_id, type_tags)`. Columns are
0-based UTF-8 **character** offsets within the line (converted to UTF-16 at the LSP boundary
if a client needs it). `occurrence_kind` is a closed ADR-003 vocab
`OccurrenceKind{definition, code_reference, comment, string, doc}`.

### Extraction: one uniform lexical scanner, not 17 grammar walks

Rather than a bespoke per-grammar occurrence walk for each of ~17 backends, occurrences are
produced by a single **language-agnostic lexical scanner**
(`src/parsing/occurrences.rs::extract_occurrences_textual`) parameterized by a small
per-language `LexConfig` (line/block/doc comment markers + string delimiters + nesting). The
scanner is one `O(n)` pass over `char_indices` that classifies every identifier as
`code_reference` / `comment` / `string` / `doc`. `LanguageBackend::extract_occurrences` is a
**provided** trait method driving the scanner with `self.lex_config()` (default C-style;
Python/Lisp/ML/Lean/TLA⁺/Metamath override it), so **every** backend produces occurrences —
this is genuinely "all identifiers, all languages", not an incremental per-backend rollout.

A cross-language keyword stoplist (`let`/`fn`/`if`/…) is skipped *in code only* (kept inside
prose), so the index stays lookup-shaped without per-language keyword tables.

### Resolution (extraction cron)

The symbol-extraction cron (`extract_and_persist_file`), after persisting `file_symbols`,
resolves each occurrence: **enclosing_symbol_id** = the innermost `file_symbols` span
containing the occurrence's line; a code identifier at a defining line is **upgraded to
`Definition`**; **resolved_target_id** = the same-file definition of the name (cross-file
resolution stays the reference resolver's job). Per-file `DELETE`-then-bulk-`INSERT`
(occurrence rows survive a `file_symbols` delete — the FK is `ON DELETE SET NULL`, so the
cron explicitly scrubs). Capped at 200 000 occurrences/file so a pathological file can't
blow the row budget.

### Type disambiguation (`x:int` vs `x:string`)

The scanner does not infer types (`type_tags` is empty for textual occurrences). Occurrence
*type* is reached via `resolved_target_id` → the target `file_symbols` row's shadow-ASR
`symbol_parameters.type_tags` / return type — which `lsp_query` op=`hover` / `type_definition`
surface. This satisfies the requirement (the capability exists end-to-end) without a second
inference engine.

### Lexical scope

`occurrences_in_scope(enclosing_symbol_id, name)` returns occurrences whose enclosing symbol
is S or a `scope_path`-prefix descendant — answering "uses of `x` within scope S". Shadowing
(same name rebound in a nested scope) is **not** resolved (documented limitation).

### Volume / indexing

At all-identifiers fidelity this is the largest table in the schema. It starts as ONE table
with targeted indexes (BRIN on the file-ordered `file_id`, btree on `name`, partial on
`definition`, GIN on `type_tags`). If row count / index bloat exceeds budget after a full
extraction, it migrates to `HASH(file_id)` declarative partitioning — a **data-driven**
decision (benchmark first), not a premature one.

## Consequences

- **Positive:** LSP references / document-highlight / scoped-reference queries become
  possible (ADR-026); the code-vs-comment-vs-string distinction is uniform across every
  indexed language; one scanner is far less code (and far less drift) than 17 grammar walks.
- **Trade-off:** textual classification is coarser than a grammar walk (it cannot tell a
  type-position identifier from a value-position one). For the occurrence index's purpose
  (find-all-mentions + provenance class) this is sufficient; precise kinds remain available
  on the `file_symbols` side.
- **Tested:** scanner unit tests (code/comment/string/doc classification, nested ML blocks,
  multi-word comments, column offsets, Python/`#` style); the v45 `step_version_is_stable`
  test; the `lsp_query_lifecycle` real-DB test exercising occurrences end-to-end.
