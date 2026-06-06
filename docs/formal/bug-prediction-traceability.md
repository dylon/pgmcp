# Bug Prediction Formal Verification Traceability

Status: focused high-use prediction slice for `bug_prediction`.

## Scope

The 31-day `mcp_tool_calls` snapshot used for this sequence showed
`bug_prediction` at 17 calls. The tool scores file metrics with the shared
bug-finding model and enriches the output with unsafe/may-panic/blocking-IO
effect symbols.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `bug_prediction` | Clamp signed result limits; fail closed on duplicate project display names; query metric rows and bug-prone effect symbols by the same resolved project id; keep score-kind metadata consistent with the scoring path; attach the project hint to durable MCP telemetry. | `tla/BugPredictionScope.tla`; `pgmcp-testing/tests/oracle_bug_prediction.rs`. |

## Issues Found And Corrected

`bug_prediction` previously cast a signed `limit` directly to `usize` for
`truncate`. Negative requests could therefore expand the output window.

Correction: limits are clamped to `1..=100`.

The tool queried metrics through `projects.name` and then performed a separate
name lookup for bug-prone effect symbols. Duplicate display names could merge
metric rows from multiple indexed projects or enrich with an arbitrary project
id.

Correction: the tool now resolves all matching project ids first, rejects
duplicates with `invalid_params`, and reuses the resolved id for metric and
effect-symbol queries.

The tool body used `expect` when extracting a raw `PgPool`. Correction: it now
returns an MCP internal error instead of panicking if invoked with a non-pool DB
client.

The MCP handler used the generic instrumentation wrapper. Correction: it now
passes `params.project` through `instrumented_tool_wrap_with_project`.

## Formal Model

`tla/BugPredictionScope.tla` models unique, duplicate, and missing project
names; negative and oversized limits; project-scoped file metrics; training
class availability; and bug-prone effect-symbol scoping.

The spec is intentionally one-shot: it picks an arbitrary request and checks the
response invariants for that call. `bug_prediction` has no cross-call state, so
this preserves the correctness obligations while avoiding state-history growth.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `NonUniqueProjectRejected` | Missing or duplicate project display names return no rows or effect symbols. |
| `RowsProjectScoped` | Every prediction row belongs to the resolved project id. |
| `EffectSymbolsProjectScoped` | Every effect symbol belongs to the resolved project id. |
| `EffectiveLimitClamped` | Every response reports the clamped limit. |
| `OutputWithinLimit` | Returned rows never exceed the effective limit. |
| `ScoreKindMatchesTrainingData` | Score-kind metadata matches the modeled trained-vs-heuristic condition. |
| `EnrichmentUsesResolvedProject` | Effect-symbol enrichment uses the same project id as the row query. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_MEMORY_MAX=768M PGMCP_TLC_JAVA_XMX=512m \
  PGMCP_TLC_WORKERS=1 timeout 60 scripts/tlc-capped.sh \
  -config docs/formal/tla/BugPredictionScope.cfg \
  docs/formal/tla/BugPredictionScope.tla
```

Result: 5 distinct states, 10 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_bug_prediction --build-jobs 1
```

Result: 4/4 passed. The regression tests cover ranking, negative-limit
clamping, oversized-limit capping, and duplicate project display-name
rejection.
