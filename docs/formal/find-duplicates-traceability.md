# Find Duplicates Formal Verification Traceability

Status: focused 4-call similarity slice for `find_duplicates`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed `find_duplicates` at 4
calls, the next uncovered tier after the 5-call tools. The tool is read-only:
it fetches bounded duplicate file pairs, clusters them with a local union-find,
and optionally surfaces cross-language signature clone pairs.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `find_duplicates` | Reject non-finite thresholds; clamp similarity/min-project/limit bounds before SQL and union-find; avoid `limit * 5` overflow; keep output bounded; apply min-similarity, language, same-repo, and live symbol/file/project consistency filters to cross-language rows; remain read-only except request stats. | `tla/FindDuplicatesBounds.tla`; `oracle_similarity_tools`. |

## Issues Found And Corrected

`limit` was used directly in `limit * 5` for SQL fetches and later cast to
`usize`. Negative or very large values could produce SQL errors, overflow, or an
unbounded `take` window.

Correction: `limit` now clamps to `1..=100`, `min_projects` clamps to a finite
range, and non-finite `min_similarity` rejects before querying. The normalized
filters are returned in the response envelope.

The cross-language signature side channel did not apply the main
`min_similarity`, `language`, or `include_same_repo` filters, and it trusted
clone-table project ids without checking that each symbol still belonged to a
live file in that project.

Correction: the cross-language query now joins through `indexed_files` and
`projects`, requires clone project ids to match the joined files, applies the
same threshold/language/same-repo filters, and keeps the same bounded fetch
window as the embedding path.

## Formal Model

`tla/FindDuplicatesBounds.tla` models accepted and rejected requests, clamped
numeric filters, bounded fetch/output windows, stale cross-language rows, and
same-repo filtering.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `FiniteSimilarityRequired` | Non-finite threshold requests reject before querying. |
| `LimitsBoundFetchAndOutput` | SQL fetch and reported cluster windows remain finite. |
| `SimilarityClamped` | Accepted thresholds are clamped into the valid similarity interval. |
| `CrossLanguageRowsAreScoped` | Reported cross-language rows satisfy threshold, language, same-repo, and project-consistency filters. |
| `ReadOnlyAdapter` | The model has no persistent-state mutation path. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh FindDuplicatesBounds.tla
```

Result: exit 0; no invariant violations; 65 distinct states and 65 states
generated at depth 5.

```bash
cargo nextest run -p pgmcp-testing --test oracle_similarity_tools --build-jobs 1
```

Result: 7/7 passed, including normalized filter bounds and stale
cross-language project-row rejection.
