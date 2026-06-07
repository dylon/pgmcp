# Quality Report Formal Verification Traceability

Status: focused reporting/aggregation slice for `quality_report`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `quality_report` in the
2-call cluster. This tool fans out across many collectors, optionally triggers
heavy crons, renders large reports, and appends GPA history. The verification
slice therefore focuses on request-boundary safety and side-effect ordering:
invalid local inputs must not start work, invalid projects must not trigger
crons or writes, refresh/history work must be bounded, and the report/history
write must share the same resolved project identity.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `quality_report` | Normalize project/format/severity inputs; reject invalid format/severity/refresh lists before project lookup or work; reject blank/missing/duplicate projects before cron/aggregate/history side effects; cap `refresh_crons` and `trend_points`; aggregate and persist history using one resolved project id; emit canonical envelope format names. | `tla/QualityReportBoundary.tla`; `quality_report_e2e` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The wrapper resolved the project separately for aggregation and history
persistence. A valid padded project name could aggregate successfully but skip
the best-effort history insert, because the history path retried lookup with
the raw string. The wrapper now trims the project once, resolves a single
project id, calls `aggregate_for_project`, and writes history for the same id.

The wrapper accepted unbounded `trend_points` and unbounded `refresh_crons`.
`trend_points` now clamps to `0..=120`, and `refresh_crons` rejects more than
eight entries or blank entries before any cron dispatch. Refresh jobs are
trimmed before being forwarded to `trigger_cron`.

`include_underlying_json=true` previously echoed the raw format string, so
aliases such as `" md "` appeared in the envelope even though Markdown was
rendered. The envelope now reports the canonical format name.

## Formal Model

`tla/QualityReportBoundary.tla` models the wrapper as a phased boundary:
local validation, one project lookup, bounded optional cron refreshes, one
aggregation for the resolved id, and one history write for that same id.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `LocalRejectsHaveNoSideEffects` | Bad format, severity, overlong refresh list, and blank refresh jobs reject before lookup, cron, aggregation, or history writes. |
| `ProjectRejectsBeforeWork` | Blank, missing, and duplicate projects perform only lookup and do not run crons, aggregate, or write history. |
| `SuccessfulCallsUseOneProjectIdentity` | Successful calls normalize to the resolved project and perform one aggregate plus one history write for that identity. |
| `RefreshCronRunsBounded` | Cron refreshes are bounded by `MAX_QUALITY_REPORT_REFRESH_CRONS` and only run after valid project resolution. |
| `TrendPointsBounded` | The persisted trend window is capped at 120 samples, plus the current run when trends are enabled. |
| `CanonicalEnvelopeFormat` | Successful JSON envelopes expose a canonical format name, not an input alias. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh QualityReportBoundary.tla
```

Result: 10 distinct states, 19 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test quality_report_e2e --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
