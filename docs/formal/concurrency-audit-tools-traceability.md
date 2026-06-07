# Concurrency Audit Tools Formal Verification Traceability

Status: focused concurrency-safety slice for `lockset_races` and
`blocking_in_async`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `lockset_races` and
`blocking_in_async` at one call each. They are low-volume tools, but they are
the direct audit surface for the requested deadlock/race/concurrency-safety
verification work. Higher-volume search, inventory, fuzzy, memory, graph, and
tracker tools already have formal slices in `docs/formal/README.md`.

| Tool | Contract | Evidence |
| --- | --- | --- |
| `lockset_races` | Resolve one non-ambiguous project; clamp scan limits to a finite window; stream regex file scans; keep mutex-symbol and effect-breakdown enrichment scoped to the resolved project; return no cross-project effects; remain read-only with no held locks. | `tla/ConcurrencyAuditScopes.tla`; filtered `tool_sota_phase5` tests once sibling dependency compilation is restored. |
| `blocking_in_async` | Resolve one non-ambiguous project; clamp scan limits to a finite window; stream eligible file content instead of collecting all rows; keep async/blocking effect intersections scoped to the resolved project; remain read-only with no held locks. | `tla/ConcurrencyAuditScopes.tla`; filtered `tool_sota_phase5` tests once sibling dependency compilation is restored. |

## Issues Found And Corrected

1. `lockset_races` returned workspace-wide effect counts.

   The regex scan and mutex-typed symbol query were project-scoped, but the
   `effect_breakdown` channel queried `symbol_effects` without joining
   `indexed_files` through the resolved project id. A concurrency audit for one
   project could therefore report effects that only existed in another project.

   Correction: `lockset_races` now uses the existing project-scoped
   `sema_helpers::effects::effect_counts(pool, project_id)` helper and sorts
   the output deterministically.

2. `lockset_races` and `blocking_in_async` accepted unbounded positive limits.

   Correction: both tools now clamp caller limits to `1..=1000` and report the
   effective limit in the JSON response.

3. `blocking_in_async` loaded every eligible file body before scanning.

   Correction: the tool now streams rows with `fetch(...).try_next()` and
   exits as soon as the effective findings limit is reached. The stream is
   explicitly dropped before effect enrichment so a small SQLx pool cannot
   self-stall on a held scan connection. The resident file content bound is one
   row plus the bounded findings vector.

## Model

`tla/ConcurrencyAuditScopes.tla` is a finite boundary model over:

- valid, blank, missing, duplicate, trimmed, negative-limit, and oversized-limit
  requests;
- project-scoped regex file rows for both tools;
- project-scoped effect rows, including an explicit other-project effect that
  must never surface in the target response;
- a streaming-content bound where at most one file body is resident during the
  scan;
- read-only execution with no locks held after return.

Key invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidOrAmbiguousNoScan` | Invalid, missing, or duplicate project requests reject before scans/enrichment. |
| `LimitBounded` | Successful calls use a finite effective limit in `1..=1000`. |
| `RegexFilesScoped` | Regex findings are from files owned by the resolved project. |
| `LocksetEffectsScoped` | `lockset_races.effect_breakdown` contains only effects present in the resolved project. |
| `BlockingEffectsScoped` | `blocking_in_async.effect_intersection` contains only symbols in the resolved project carrying both `async` and `blocking_io`. |
| `StreamingScanBound` | File-content scanning has at most one resident file row. |
| `ReadOnlyAndNoLocksHeld` | The tools do not write and do not retain locks after returning. |

## Verification Run 2026-06-07

TLC, using the RSS-capped wrapper:

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh ConcurrencyAuditScopes.tla
```

Result: 11 distinct states, 16 generated states, no invariant violations.

Rust:

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 \
  lockset_races_scopes_effect_breakdown_to_project --build-jobs 1
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 \
  blocking_in_async_streams_and_clamps_limit --build-jobs 1
```

Result: blocked before pgmcp tests by sibling `libgrammstein` compile errors
(`PersistentARTrieChar::read` trait import and `Option<i64>::copied()`).
