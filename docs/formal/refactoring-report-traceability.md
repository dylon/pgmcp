# Refactoring Report Formal Verification Traceability

Status: focused read-only similarity/reporting slice for `refactoring_report`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed `refactoring_report` at 4
calls, one of the next uncovered tools after `ontology_invariants_for_file`.
The tool is read-only: it queries duplicate file pairs, clusters them by
project span, and returns extraction candidates.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `refactoring_report` | Reject non-finite similarity; normalize and bound similarity, language, `min_projects`, and output limit; compute a bounded fetch window without overflow; return at most the effective limit; remain read-only with no persistent locks. | `tla/RefactoringReportBounds.tla`; `mcp_tool_smoke`. |

## Issues Found And Corrected

The tool accepted raw `min_similarity`, `min_projects`, and `limit` values.
Negative limits could be cast to huge `usize` values for output truncation, and
`limit * 5` could overflow before querying duplicate file pairs.

Correction: `limit` now clamps to `1..=100`, `min_projects` to `1..=128`, and
the internal duplicate-pair fetch window is computed with saturating
multiplication.

The tool also accepted non-finite similarity values and unbounded language
filters.

Correction: `min_similarity` must be finite and clamps to `0..=1`; language is
trimmed, lowercased, and capped at 64 bytes.

## Formal Model

`tla/RefactoringReportBounds.tla` models valid clamped requests, non-finite
similarity rejection, oversized language rejection, low limits, high limits,
and bounded output candidate counts.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Non-finite similarity and oversized language requests reject. |
| `SimilarityIsFiniteAndBounded` | Accepted similarity is finite and within the normalized range. |
| `LanguageNormalizedAndBounded` | Accepted language filters are either absent or normalized. |
| `LimitsAndFetchAreBounded` | Output limit and duplicate-pair fetch limit remain finite. |
| `MinProjectsBounded` | Project-span filters remain within a finite range. |
| `CandidatesDoNotExceedLimit` | Candidate output cannot exceed the effective limit. |
| `ReadOnlyNoHeldLock` | The model has no persistent write or held-lock path. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh RefactoringReportBounds.tla
```

Result: exit 0; no invariant violations; 4 distinct states and 8 states
generated at depth 1.

```bash
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke refactoring_report --build-jobs 1
```

Result: 4/4 passed for the filtered refactoring-report slice.
