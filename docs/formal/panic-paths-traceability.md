# Panic Paths Formal Verification Traceability

Status: focused safety/concurrency-adjacent slice for `panic_paths`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed `panic_paths` at 4 calls,
one of the next uncovered tools after `memory_unified_search`. The tool is
read-only: it resolves one project, reads `function_metrics.panic_paths`, joins
function symbols to indexed files, and returns the functions most likely to
panic on unexpected input. It also exposes a structured `may_panic` effect
channel derived from `symbol_effects`.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `panic_paths` | Resolve a unique nonblank project; validate `entry_filter`; clamp result limits; reject stale/corrupt metric rows whose metric project, symbol file, and indexed-file project do not agree; scope `may_panic` effect rows to the resolved project; remain read-only with no persistent locks. | `tla/PanicPathsScope.tla`; `tool_sota_phase5`. |

## Issues Found And Corrected

The tool accepted any `entry_filter` string and silently treated unknown values
as `any`. This made typos look successful while widening the query.

Correction: `entry_filter` is trimmed and validated against the closed set
`any | pub | module | private`; invalid filters fail closed.

The tool passed the caller's raw limit directly into SQL. Zero or negative
limits could produce bad SQL behavior, and very large values had no response
cap.

Correction: `limit` now clamps to `1..=1000` and the effective value is
reported in the JSON response.

The metric query joined `function_metrics` to `file_symbols` by function id and
then to `indexed_files` by the symbol's file id, while only filtering
`function_metrics.project_id`. A stale metric row with the requested
`project_id` but a function/file from another project could leak a foreign
path.

Correction: the join now requires `file_symbols.file_id = function_metrics.file_id`,
`indexed_files.id = function_metrics.file_id`, and
`indexed_files.project_id = function_metrics.project_id = $1`.

## Formal Model

`tla/PanicPathsScope.tla` models valid, blank-project, ambiguous-project,
invalid-filter, low-limit, stale-metric, private/public visibility, and
project-scoped effect-channel cases.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsDoNotQuery` | Blank/ambiguous projects and invalid filters reject without querying. |
| `LimitsAreBounded` | Accepted requests use a finite positive result limit. |
| `EntryFilterRespected` | Reported functions satisfy the normalized visibility filter. |
| `ReportedFunctionsScoped` | Every reported function has a live metric/symbol/file chain in the resolved project. |
| `StaleMetricsRejected` | Stale metric rows cannot appear in `functions`. |
| `EffectsScoped` | `may_panic` effect rows belong to the resolved project. |
| `ReadOnlyNoHeldLock` | The model has no persistent write or held-lock path. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh PanicPathsScope.tla
```

Result: exit 0; no invariant violations; 5 distinct states and 10 states
generated at depth 1.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 panic_paths --build-jobs 1
```

Result: 3/3 passed for the filtered panic-path slice.
