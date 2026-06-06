# Centrality Analysis Formal Verification Traceability

Status: focused high-use graph slice for `centrality_analysis`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`centrality_analysis` at 13 calls. The tool ranks file-level graph metrics and
adds effect/cross-project context, so all data sources must share one resolved
project identity.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `centrality_analysis` | Reject blank project names; fail closed on duplicate project display names; validate centrality metrics; clamp signed result limits; query metric rows by the resolved project id and matching file project id; enrich effects/cross-project blocks with the same project id; return a JSON envelope for valid projects with no metric rows. | `tla/CentralityAnalysisScope.tla`; `pgmcp-testing/tests/oracle_centrality_analysis.rs`. |

## Issues Found And Corrected

The tool joined through `projects.name` directly. Duplicate display names could
merge metric rows from multiple indexed projects, while effect and
cross-project enrichment performed separate name lookups that could resolve a
different duplicate row.

Correction: the tool now trims the project name, resolves matching project ids
once, rejects duplicates with `invalid_params`, and reuses the single resolved
id for metric rows, effect enrichment, and cross-project dependency context.
The metric query also requires both `file_metrics.project_id` and
`indexed_files.project_id` to match that resolved id.

Unknown metrics previously fell through to PageRank ordering. Correction:
metric names are trimmed and validated against `pagerank`, `betweenness`,
`degree`, and `all`; blank optional metrics use `all`.

Signed limits were passed directly to SQL. Correction: limits are clamped to
`1..=200` and the effective limit is reported in the response envelope.

Valid projects with no graph metrics previously returned a plain text message,
which dropped cross-project context. Correction: they now return the normal JSON
envelope with `files: []`.

## Formal Model

`tla/CentralityAnalysisScope.tla` models blank and whitespace project names,
trimmed project/metric values, duplicate and missing projects, valid metrics,
an invalid metric, negative and oversized limits, an empty project, and a drift
row where `file_metrics.project_id` and `indexed_files.project_id` disagree.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsRejected` | Blank, duplicate, missing, and invalid-metric requests return no rows. |
| `MetricValidatedAndNormalized` / `ProjectNormalized` | Accepted requests use normalized input values. |
| `RowsProjectScoped` / `NoCrossProjectMetricFileDrift` | Metric rows and files both belong to the resolved project id. |
| `EffectiveLimitClamped` / `OutputWithinLimit` | Returned rows never exceed the clamped limit. |
| `EnrichmentUsesResolvedProject` | Metrics, effect counts, and cross-project lookups share one project id. |
| `EmptyProjectReturnsJsonEnvelope` | A valid project with no metrics still returns a scoped JSON envelope. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh CentralityAnalysisScope.tla)
```

Result: 10 distinct states, 20 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_centrality_analysis --build-jobs 1
```

Result: 5/5 passed. The focused suite covers pinned PageRank ordering,
project/metric trimming, negative-limit clamping, oversized-limit capping,
invalid metric rejection, and duplicate project display-name rejection.
