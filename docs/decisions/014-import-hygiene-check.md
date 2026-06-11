# ADR-014: Import-hygiene check (imports inside function bodies)

- **Status:** Accepted — implemented 2026-06-10
- **Supersedes / relates to:** ADR-013 (disk-fallback symbol extraction),
  ADR-011 (shadow-ASR concurrency analysis). Builds directly on the shadow-ASR
  symbol/reference layer.

## Context

Imports buried inside function/method bodies — rather than at the top of the file,
module, or test module — are a recurring smell. They hide a scope's true
dependencies, defeat "see all deps at a glance," and breed duplicated `use` lines
(the same import re-typed in many function bodies). `cargo fmt`/rustfmt orders and
groups imports but **never hoists a `use` out of a function body**, so this
*placement* violation slips through every formatter.

The policy to encode (Rust-centric, but generalized):

- Source imports belong at the **top of the file**.
- Test imports belong at the **top of the test module** (`mod tests { … }`), where
  the test functions live.
- Integration-test imports belong at the **top of the file**.
- Imports inside function bodies — **including test-function bodies** — are
  violations.

## Decision

Add an **import-hygiene check** as a *shadow-ASR analysis* — not a regex pass — over
data the symbol-extraction pipeline already persists, surfaced two ways:

1. A **best-practices-sweep collector** `collect_import_hygiene`
   (`src/quality/collectors/hygiene.rs`), registered in the `quality_report`
   light-wave aggregator (`src/quality/aggregate.rs`). It folds automatically into
   `quality_report`, `quality_trend`, `quality_forecast`, the `quality-history`
   cron, and the proactive digest.
2. A **standalone `import_hygiene` MCP tool** (`src/mcp/tools/tool_import_hygiene.rs`)
   for targeted per-project runs, returning `violations[]` + a `by_file` rollup.

Both call one shared query, `nested_import_violations`
(`src/db/queries/symbols.rs`).

### The detection rule

> An `import_use` reference is a **violation** iff its resolved `source_symbol_id`
> points at a `file_symbols` row of a **callable kind** (`function`, `method`,
> `lambda`).

This single predicate encodes the whole policy, across every backend language:

```
 use / import location              resolved source_symbol_id      verdict
 ─────────────────────────────────┼──────────────────────────────┼──────────
 file / crate root                 │ NULL (no enclosing symbol)   │ OK
 mod tests { use super::*; } top   │ the `module`                 │ OK
 fn f() { use std::fs; }           │ the `function`               │ FLAGGED
 #[test] fn t() { use …; }         │ the `function`               │ FLAGGED
 impl method body `use`            │ the `function` (method)      │ FLAGGED
 def f(): import os   (Python)     │ the `function`               │ FLAGGED
```

Type-definition containers (`struct`/`enum`/`trait`/`class`/`impl`/`namespace`) and
`module` are not callable, so a top-of-scope import never flags.

### Why this mechanism (and not the alternatives)

The pipeline already computes, per import, *which symbol's body encloses it*:

```
 src/cron/symbol_extraction.rs
   581  references.extend(imports_as_references(backend.extract_imports(content)))
        → import_use rows folded in with source_symbol_id = None
   649  bulk_insert_file_symbols → DB symbol_ids
   746  resolve_source_symbol_ids(&symbols, &symbol_ids, &mut references)
        → every still-None reference (incl. import_use) gets source_symbol_id =
          the smallest-range symbol whose [start_line,end_line] covers source_line
   749  bulk_insert_symbol_references → persisted WITH the resolved FK
```

So the check is a direct FK join (`symbol_references.source_symbol_id →
file_symbols.id`) filtered to callable kinds. Rejected alternatives:

- **New `file_imports` table + re-extraction** — unnecessary: imports are already
  persisted as `import_use` rows, and their enclosing scope is already resolved.
- **Line-range containment join at query time** — redundant: `resolve_source_symbol_ids`
  is exactly that containment resolution, already materialized into the indexed
  `source_symbol_id` FK. (This *was* the right call before that resolve pass existed;
  it no longer is.)
- **Regex scan of file content** — loses the scope tree the shadow-ASR layer exists
  to provide; would mis-handle nested scopes, macros, and test modules.

**No new table, no migration, no extraction change.**

### Severity & duplication

`dup_count` = how many violations in the same file share a `target_raw` (the same
import re-typed across function bodies). Severity rides it: `1` → Low, `≥2` →
Medium, `≥4` → High. This makes the egregious duplicated case — the one whose fix
removes several lines — rank above an isolated nested import.

## Consequences

- **All backend languages** are flagged uniformly (decision: broad coverage).
  Python lazy-imports and JS dynamic `require()` inside functions are sometimes
  idiomatic, so expect some non-Rust noise; the `dup_count`→severity ramp keeps
  singletons Low. A per-language severity/allowlist knob can be added behind config
  later if the signal proves noisy.
- **Depends on the resolve pass** having populated `source_symbol_id`. It runs on
  every extraction, and a project missing `import_use` rows is force-rescanned
  (`project_missing_import_refs`), so steady state is self-healing. A project not yet
  re-extracted under the resolve pass simply **under-reports** (no false positives).
- **Ordering/grouping** of imports stays the formatter's job; this check targets only
  the placement violation formatters miss plus the duplication that hoisting removes.

## Verification

- Unit/integration (`pgmcp-testing/tests/tool_import_hygiene.rs`): file-top and
  `mod tests { … }`-top imports NOT flagged; fn-body and `#[test]`-fn-body imports
  flagged; the same import in two bodies → `dup_count == 2` / Medium; a Python
  `def f(): import os` flagged (cross-language); unknown project soft-fails with
  `health.symbols_present:false`. The test drives the tool via
  `call_tool_cli("import_hygiene", …)` (also satisfies the dispatch coverage gate).
- The `quality_report` e2e asserts an `import_hygiene` finding lands in the Hygiene
  bucket (Engineering pillar).
- Full `./scripts/verify.sh` gate, then a live check: trigger `symbol-extraction`,
  call `import_hygiene`, and confirm the finding folds into `quality_report`.
