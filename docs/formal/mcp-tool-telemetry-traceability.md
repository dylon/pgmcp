# MCP Tool Telemetry Formal Verification Traceability

Status: focused high-use inventory slice for `mcp_tool_telemetry`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`mcp_tool_telemetry` itself at 13 calls. The tool reads durable
`mcp_tool_calls` history and exposes summary, ranking, histogram, error-rate,
and raw-row aggregations.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `mcp_tool_telemetry` | Normalize optional tool/client/project filters; clamp the lookback window to `1..=44640` minutes; clamp raw result limits to `1..=1000`; reject unknown aggregations; reuse the same normalized filters across every aggregation; exclude empty project rows from `top_projects`; attach normalized project hints to durable telemetry. | `tla/McpToolTelemetryFilters.tla`; `pgmcp-testing/tests/query_smoke_mcp_tools.rs`. |

## Issues Found And Corrected

Optional filters were passed directly to SQL. A request such as
`project: " pgmcp "` did not match `project='pgmcp'`, and the response envelope
echoed raw filters.

Correction: the tool now trims optional `tool`, `client_name`, and `project`
filters once at the boundary. Blank optional filters are treated as absent, and
the normalized values are used by all SQL aggregations and echoed in the
response envelope.

Aggregation names are also trimmed; a blank aggregation defaults to `summary`.
Unknown aggregation names still fail closed with `invalid_params`. The handler
now passes a trimmed/nonblank project hint to the instrumentation wrapper.

The tool already clamped `since_minutes` and raw `limit`; the focused test now
pins negative raw-limit clamping.

## Formal Model

`tla/McpToolTelemetryFilters.tla` models a small durable telemetry table with
two projects plus an empty project field, whitespace-padded filters, blank
filters, low/high lookback windows, low/high raw limits, all key aggregation
classes, and an invalid aggregation.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidAggregationRejected` | Unknown aggregation modes return no rows. |
| `AggregationNormalizedAndValidated` | Accepted aggregation names are normalized and in the closed set. |
| `FiltersNormalized` | Optional filter fields are trimmed or treated as absent. |
| `SinceClamped` / `RawLimitClamped` | Lookback and raw-row limits stay in their bounded domains. |
| `RowsMatchNormalizedFilters` | Every returned row satisfies the normalized filters. |
| `TopProjectsExcludeEmptyProject` | `top_projects` does not report empty project names. |
| `RawOutputWithinLimit` | Raw telemetry rows never exceed the effective raw limit. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh McpToolTelemetryFilters.tla)
```

Result: 8 distinct states, 16 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test query_smoke_mcp_tools mcp_tool_telemetry --build-jobs 1
```

Result: 2/2 passed. The filtered smoke run covers the empty-table case and
project filtering across `top_tools`, `top_callers`, `top_projects`,
`error_rate`, `summary`, `histogram`, and `raw`, including trimmed filters and
negative raw-limit clamping.
