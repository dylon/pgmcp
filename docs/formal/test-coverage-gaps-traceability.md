# Test Coverage Gaps Formal Verification Traceability

Status: focused high-use topic/quality slice for `test_coverage_gaps`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`test_coverage_gaps` at 15 calls. The tool reports implementation topics with
weak corresponding test coverage and opportunistically enriches the result with
indexed real coverage reports and effect-symbol counts.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `test_coverage_gaps` | Reject empty projects; fail closed on duplicate project display names; use one resolved project id for topic rows, real coverage artifacts, and effect enrichment; preserve the documented status threshold table; attach the project hint to durable MCP telemetry. | `tla/TestCoverageGapsScope.tla`; `pgmcp-testing/tests/oracle_test_coverage_gaps.rs`. |

## Issues Found And Corrected

The production tool looked up `projects.name` independently for real coverage
and effect enrichment, and the topic-proxy query joined by project display name.
Duplicate display names could therefore merge topic rows or enrich from an
arbitrary project.

Correction: the tool now resolves the project display name once. Duplicate names
return `invalid_params`; missing names preserve the existing no-data guidance.
When a raw pool is available, topic coverage is queried by the resolved
`project_id`, and the same id is reused for real coverage and effect counts.
The mocked `DbClient` path remains available for unit oracles.

The tool accepted blank project names. Correction: blank/whitespace project
names are rejected before any query.

The handler used generic instrumentation. Correction: it now passes
`params.project` through `instrumented_tool_wrap_with_project`.

## Formal Model

`tla/TestCoverageGapsScope.tla` models blank, duplicate, missing, and uniquely
resolved project requests; topic rows from two possible projects; real coverage
presence; and effect enrichment presence.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankProjectsRejected` | Empty/whitespace projects are rejected. |
| `DuplicateProjectsRejected` | Duplicate display names return no rows, real coverage, or effects. |
| `MissingProjectsHaveNoScopedData` | Missing projects return no scoped data. |
| `TopicRowsProjectScoped` | Every returned topic row belongs to the resolved project id. |
| `CoverageAndEffectsUseResolvedProject` | Real coverage and effects use the same resolved project id. |
| `StatusClassificationCorrect` | Status labels match the integer threshold table. |
| `TelemetryProjectNormalized` | Telemetry/output project text is the trimmed request project. |
| `CoverageSourceMatchesRealCoverage` | `coverage_source` matches real coverage presence. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh TestCoverageGapsScope.tla)
```

Result: 13 distinct states, 19 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_test_coverage_gaps --build-jobs 1
```

Result: 4/4 passed. The oracle covers threshold classification, sort order,
blank-project rejection, and duplicate project display-name rejection.
