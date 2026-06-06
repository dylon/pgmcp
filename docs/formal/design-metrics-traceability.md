# Design Metrics Formal Verification Traceability

Status: focused high-use architecture slice for `design_metrics`.

## Scope

The 31-day `mcp_tool_calls` snapshot used for this sequence showed
`design_metrics` at 18 calls. The tool resolves a project, optionally filters by
module/file path, computes per-file design metrics, and enriches with
function-metric/effect rows.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `design_metrics` | Clamp signed result limits; reject invalid scopes; fail closed on duplicate project display names; query file rows, function aggregates, and effect enrichment by the same resolved project id; treat module/file path filters as literal bound parameters; attach the project hint to durable MCP telemetry. | `tla/DesignMetricsScope.tla`; `pgmcp-testing/tests/oracle_design_metrics.rs`. |

## Issues Found And Corrected

`design_metrics` previously cast a signed `limit` directly to `usize` for
`truncate`. Negative requests could therefore expand the output window instead
of bounding it.

Correction: limits are clamped to `1..=100`.

The tool queried files through `projects.name` and looked up function/effect
enrichment through a separate name lookup. Duplicate display names could merge
file rows from multiple indexed projects or enrich with an arbitrary project id.

Correction: the tool now resolves all matching project ids first, rejects
duplicates with `invalid_params`, and reuses the resolved id for file,
function-metric, and effect queries.

The `module` scope documented by the parameter schema was not implemented; only
`directory` got prefix behavior. Path filters were also formatted into SQL
strings.

Correction: `module` and `directory` both use literal bound-prefix filtering;
`file` uses literal equality; invalid scopes fail closed.

The MCP handler used the generic instrumentation wrapper. Correction: it now
passes `params.project` through `instrumented_tool_wrap_with_project`.

## Formal Model

`tla/DesignMetricsScope.tla` models unique, duplicate, and missing project
names; valid/invalid scopes; literal path prefixes including `%`; negative and
oversized limits; and reuse of the resolved project id for enrichment.

The spec is intentionally one-shot: it picks an arbitrary request and checks the
response invariants for that call. `design_metrics` has no cross-call state, so
this preserves the correctness obligations while avoiding state-history growth.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsRejected` | Missing/duplicate projects and invalid scopes return no rows. |
| `RowsProjectScoped` | Every row belongs to the resolved project id. |
| `ScopeFilterSound` | Module/file filters are honored literally. |
| `EffectiveLimitClamped` | Every response reports the clamped limit. |
| `OutputWithinLimit` | Returned rows never exceed the effective limit. |
| `EnrichmentUsesResolvedProject` | Function/effect enrichment uses the same project id as the row query. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_MEMORY_MAX=768M PGMCP_TLC_JAVA_XMX=512m \
  PGMCP_TLC_WORKERS=1 timeout 60 scripts/tlc-capped.sh \
  -config docs/formal/tla/DesignMetricsScope.cfg \
  docs/formal/tla/DesignMetricsScope.tla
```

Result: 9 distinct states, 18 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_design_metrics --build-jobs 1
```

Result: 5/5 passed. The regression tests cover per-file output shape,
negative-limit clamping, oversized-limit capping, module-prefix filtering, and
duplicate project display-name rejection.
