# Technical Debt Analysis Formal Verification Traceability

Status: focused high-use prediction slice for `technical_debt_analysis`.

## Scope

The 31-day `mcp_tool_calls` snapshot used for this sequence showed
`technical_debt_analysis` at 18 calls. The tool scores files by TODO/FIXME-style
marker density, branch-count complexity, churn, fix ratio, and file size.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `technical_debt_analysis` | Clamp signed result limits; fail closed on duplicate project display names; query file rows and effect enrichment by the same resolved project id; honor `include_todos`; attach the project hint to durable MCP telemetry. | `tla/TechnicalDebtAnalysisScope.tla`; `pgmcp-testing/tests/oracle_technical_debt_analysis.rs`. |

## Issues Found And Corrected

`technical_debt_analysis` previously cast a signed `limit` directly to `usize`
for `truncate`. Negative requests could therefore expand the output window.

Correction: limits are clamped to `1..=100`.

The tool queried files through `projects.name` and then performed a separate
name lookup for effect enrichment. Duplicate display names could merge file
rows from multiple indexed projects or enrich with an arbitrary project id.

Correction: the tool now resolves all matching project ids first, rejects
duplicates with `invalid_params`, and reuses the resolved id for file and effect
queries.

The tool body used `expect` when extracting a raw `PgPool`. Correction: it now
returns an MCP internal error instead of panicking if invoked with a non-pool DB
client.

The MCP handler used the generic instrumentation wrapper. Correction: it now
passes `params.project` through `instrumented_tool_wrap_with_project`.

## Formal Model

`tla/TechnicalDebtAnalysisScope.tla` models unique, duplicate, and missing
project names; negative and oversized limits; `include_todos`; project-scoped
files; and effect enrichment project identity.

The spec is intentionally one-shot: it picks an arbitrary request and checks the
response invariants for that call. `technical_debt_analysis` has no cross-call
state, so this preserves the correctness obligations while avoiding
state-history growth.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `NonUniqueProjectRejected` | Missing or duplicate project display names return no rows. |
| `RowsProjectScoped` | Every row belongs to the resolved project id. |
| `EffectiveLimitClamped` | Every response reports the clamped limit. |
| `OutputWithinLimit` | Returned rows never exceed the effective limit. |
| `TodosDisabledSuppressesMarkerCount` | `include_todos=false` suppresses marker totals. |
| `EnrichmentUsesResolvedProject` | Effect enrichment uses the same project id as the row query. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_MEMORY_MAX=768M PGMCP_TLC_JAVA_XMX=512m \
  PGMCP_TLC_WORKERS=1 timeout 60 scripts/tlc-capped.sh \
  -config docs/formal/tla/TechnicalDebtAnalysisScope.cfg \
  docs/formal/tla/TechnicalDebtAnalysisScope.tla
```

Result: 5 distinct states, 10 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_technical_debt_analysis --build-jobs 1
```

Result: 4/4 passed. The regression tests cover TODO/churn ranking,
negative-limit clamping, oversized-limit capping, and duplicate project
display-name rejection.
