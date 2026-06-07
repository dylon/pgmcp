# Security Audit Tools Formal Verification Traceability

Status: focused security-audit slice for `taint_analysis`,
`injection_candidates`, `crypto_misuse`, and `unsafe_deserialization`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed recent use of the remaining
Phase 6 security-audit tools, each without a dedicated formal traceability
slice. This pass verifies their direct MCP boundaries: local request
normalization, project-id scoping, bounded scans, predicate-before-cap
filtering, bounded enrichment, and read-only/concurrency behavior.

Local correctness obligations:

| Tools | Obligations | Evidence |
| --- | --- | --- |
| `taint_analysis`, `injection_candidates` | Normalize project names; clamp limits to `1..=500`; stream dataflow-capable file contents; bound intraprocedural and interprocedural findings; apply injection sink-kind filtering before the cap; stream heuristic scans; reject invalid injection `kind`; bound effect-symbol enrichment; return normalized effective parameters; execute read-only with no retained locks. | `tla/SecurityAuditBoundary.tla`; `tool_sota_phase6` once sibling dependency compilation is restored. |
| `crypto_misuse`, `unsafe_deserialization` | Normalize project names; clamp limits to `1..=500`; stream AST-rule-capable file contents; apply AST category filtering before the cap; stream regex fallback scans; bound effect-symbol enrichment; return normalized effective parameters; execute read-only with no retained locks. | `tla/SecurityAuditBoundary.tla`; `tool_sota_phase6` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The dataflow and AST helpers fetched every matching file body, built all
findings, and only then truncated tool responses. They now stream indexed file
content in deterministic `id` order and stop once the relevant bounded channel
is full.

`injection_candidates` treated unknown `kind` values as `all`. It now accepts
only `all`, `sql`, or `shell`, after trimming, and rejects anything else before
scanning.

Filtering now happens before caps for specialized tools. Injection sink-kind
filtering is passed into the dataflow scan, and AST category filtering is passed
into the AST scan, so unrelated findings cannot consume the limit before a
matching security finding is seen.

The tools now clamp caller limits to `1..=500` before work starts and return the
normalized project and effective limit in their JSON envelopes.

Effect-symbol enrichment was project-scoped but unbounded. This slice adds
bounded effect-query variants and uses them from the audited tools, with an
`effect_symbol_limit` field in the response. The any-effect query was also
rewritten through a deduping subquery so ordering after `DISTINCT` is valid
PostgreSQL.

## Formal Model

`tla/SecurityAuditBoundary.tla` models each direct tool call as request
validation, project lookup, bounded streaming scan, stream drop, bounded
effect-symbol enrichment, and response construction.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidInputsDoNotScan` | Invalid projects and invalid injection kinds do not scan files or run enrichment. |
| `ProjectRejectsBeforeScan` | Blank, missing, and duplicate projects reject before scan/enrichment. |
| `BadKindRejectsBeforeScan` | Unknown injection kinds reject after project lookup and before scan/enrichment. |
| `EffectiveBoundsHold` | Findings and effect-symbol rows stay within effective caps. |
| `StreamingMemoryBound` | Each content scan holds at most one file body at a time. |
| `EnrichmentAfterStreamDrop` | Effect enrichment cannot run while a content stream is held. |
| `ScopedRowsOnly` | Cross-project rows are not reported. |
| `PredicateBeforeCap` | Security predicates are applied before the cap for specialized tools. |
| `ReadOnly` / `NoRetainedLocks` | The tools perform no writes and retain no locks. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh SecurityAuditBoundary.tla
```

Result: 10 distinct states, 19 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase6 --build-jobs 1
```

Result: pending until sibling `libdictenstein` / `libgrammstein` compilation is
restored; Rust workspace builds are intentionally not run during this formal-only
blocker window.
