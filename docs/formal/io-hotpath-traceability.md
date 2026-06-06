# IO Hotpath Formal Verification Traceability

Status: focused 4-call concurrency/performance slice for `io_hotpath`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed `io_hotpath` at 4 calls,
the next uncovered tool after `find_duplicates`. The tool is read-only: it
scans indexed file content for disk/network/database I/O calls, weights matches
by precomputed centrality, and returns I/O effect-symbol hints.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `io_hotpath` | Resolve a unique nonblank project before scanning; clamp result limits; keep regex scan and effect-symbol output bounded; reject stale `file_metrics` rows whose `file_id` belongs to another project; report normalized project/limit metadata; remain read-only except request stats. | `tla/IoHotpathScope.tla`; `tool_sota_phase5`. |

## Issues Found And Corrected

The tool used `limit.max(0) as usize`, so zero or negative limits could return
an empty response and very large values had no finite response cap.

Correction: `limit` now clamps to `1..=1000`, and the response exposes the
effective limit plus truncation metadata.

The centrality lookup joined `file_metrics` to `indexed_files` only by
`file_id`. A stale or manually corrupted metrics row with `file_metrics.project_id`
set to the requested project but `file_id` pointing to another project's file
could weight a same-relative-path hit incorrectly.

Correction: the join now requires `indexed_files.project_id = file_metrics.project_id`
and both equal the resolved project id.

## Formal Model

`tla/IoHotpathScope.tla` models valid, blank, and ambiguous project requests,
limit clamping, scan caps, stale metrics, scoped files, bounded effect symbols,
and read-only execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectsDoNotScan` | Blank/ambiguous/unknown projects reject before scanning. |
| `LimitsAndScansAreBounded` | Effective limit, scan count, file output, and effect-symbol output are finite. |
| `OnlyProjectFilesReported` | Reported files belong to the resolved project. |
| `StaleMetricsRejected` | Metric rows only contribute when both metric and file project ids agree. |
| `EffectsScoped` | Effect-symbol hints belong to the resolved project. |
| `ReadOnlyAdapter` | The model has no persistent-state mutation path. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh IoHotpathScope.tla
```

Result: exit 0; no invariant violations; 326 distinct states and 326 states
generated at depth 6.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 io_hotpath --build-jobs 1
```

Result: 2/2 passed, covering normalized/clamped output and stale cross-project
metric rejection.
