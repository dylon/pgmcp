# Design Smell Detection Formal Verification Traceability

Status: focused high-use architecture slice for `design_smell_detection`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed `design_smell_detection` at 5
calls. The tool is read-only and derives findings from indexed files, metrics,
topic assignments, co-change data, and effect-symbol enrichment.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `design_smell_detection` | Normalize project and smell filters; reject blank/duplicate projects and invalid smell names; bound result limits; scope file, metric, topic, co-change, effect, and fix-generation paths to the same resolved project id; ignore stale metric rows whose `file_metrics.project_id` disagrees with the indexed file owner. | `tla/DesignSmellDetectionScope.tla`; `oracle_design_smell_detection`. |

## Issues Found And Corrected

The tool joined `projects` by display name and accepted the first matching row.
Duplicate display names could therefore merge unrelated files and metrics.

Correction: the tool now trims the project name and resolves it through
`project_id_or_err`, which fails closed for blank, missing, or non-unique names.

The metric join used only `file_metrics.file_id = indexed_files.id`. If a stale
metric row carried a mismatched `project_id`, the smell detector could report a
finding from inconsistent project state.

Correction: the join now requires `fm.project_id = f.project_id`; topic rows,
co-change pairs, effect enrichment, and recommended-fix generation all use the
same resolved project id/name.

The `smells` filter and `limit` were not validated. Invalid smells silently
produced confusing empty output, and negative limits were cast to large `usize`
values.

Correction: smell names are validated against the five supported smell kinds,
empty explicit filters reject, and limits clamp to `1..=1000`.

## Formal Model

`tla/DesignSmellDetectionScope.tla` models valid and invalid requests over
duplicate projects, invalid smell filters, negative limits, valid metric rows,
and stale cross-project metric rows.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsRejectNoRows` | Invalid request shapes reject and return no smells. |
| `DuplicateProjectsReject` | Non-unique project display names fail closed. |
| `SmellsStayInResolvedProject` | Emitted smells are scoped to the resolved project id. |
| `StaleMetricRowsIgnored` | Metric rows whose project id disagrees with the file owner cannot produce smells. |
| `OnlyRequestedSmellsReturned` | Filtered requests cannot emit unrequested smell kinds. |
| `LimitBounded` | Result counts respect the normalized finite limit. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh DesignSmellDetectionScope.tla
```

Result: 651 distinct states, 651 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_design_smell_detection --build-jobs 1
```

Result: 3/3 passed, including normalized/bounded filters, invalid-smell
rejection, duplicate-project fail-closed behavior, stale-metric rejection, and
the unstable-dependency positive oracle.
