# Prediction Reporting Formal Verification Traceability

Status: focused high-use prediction/reporting slice for `code_on_fire` and
`documented_tech_debt`.

## Scope

The current telemetry ordering put `code_on_fire` and `documented_tech_debt`
next in the prediction/reporting cluster after the earlier
`bug_prediction`, `technical_debt_analysis`, and related quality-tool slices.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `code_on_fire` | Trim project/mode inputs; resolve exactly one project; clamp signed limits to `1..=200`; reject unknown modes; reject non-finite/out-of-range quartiles; read file metrics and function metrics only when they agree with the resolved project; enrich with effect counts from the same project id; return a stable JSON envelope for empty results. | `tla/PredictionReportingScope.tla`; `pgmcp-testing/tests/tool_prediction_integration.rs`. |
| `documented_tech_debt` | Trim project/filter inputs; resolve exactly one project; clamp signed limits to `1..=1000`; normalize output format/category/severity/kind/language; reject unknown format/category/severity and negative `min_age_days`; scope file scans to the resolved project id; ensure returned findings satisfy normalized filters. | `tla/PredictionReportingScope.tla`; `pgmcp-testing/tests/tool_documented_tech_debt.rs`. |

## Issues Found And Corrected

Both tools previously accepted several raw string parameters without
normalizing whitespace. Correction: project names and enum-like filters are now
trimmed before validation and reporting.

`code_on_fire` queried by project display name and used raw pool extraction.
Correction: it now resolves exactly one project id, fails closed on ambiguous
names, uses structured MCP errors instead of `expect`, and scopes all SQL by
the resolved id.

`code_on_fire` also joined derived metrics by file id alone. A stale
`function_metrics.project_id` or `file_metrics.project_id` could therefore be
reported through a file that currently belonged to the requested project.
Correction: the query now requires file metrics and function metrics to agree
with the resolved project. The regression test seeds deliberately inconsistent
metric rows and asserts they do not appear.

`documented_tech_debt` did not reject invalid filter values uniformly.
Correction: unsupported `format`, `category`, and `severity` values now fail
closed; negative `min_age_days` is rejected; blank path-exclude entries are
ignored before glob compilation.

## Concurrency Boundary

This slice is read-only after request validation: neither tool mutates shared
Rust state or spawns worker threads, and the implementation adds no locks.
The race-safety property checked here is project identity stability at the row
boundary: after a single project id is resolved, all returned rows are selected
by that id and derived metric rows must agree with it. Concurrent indexing may
change which rows exist for a later call, but it cannot cause this call to
return rows from another project id.

## Formal Model

`tla/PredictionReportingScope.tla` is a one-shot request model over unique,
duplicate, and missing project names; signed limit values; trimmed and invalid
enum-like parameters; valid and invalid quartiles; stale cross-project metric
rows; and documented-debt findings with category/severity/kind/language
filters.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `UniqueProjectRequired` | Blank, missing, and duplicate project names return no rows. |
| `RejectedRequestsDoNotReturnRows` | Invalid requests fail closed. |
| `CodeModeValidated` | `code_on_fire` accepts only `intersect`, `union`, or `max` after normalization. |
| `CodeQuartilesValidated` | Quartile parameters are bounded before use. |
| `CodeLimitClamped` / `CodeOutputWithinLimit` | `code_on_fire` limits stay in `1..=200` and bound output. |
| `CodeMetricRowsProjectConsistent` | Returned hotspot rows have matching file and metric project ids. |
| `DebtFiltersValidatedAndNormalized` | `documented_tech_debt` filters are normalized and checked. |
| `DebtMinAgeNonnegative` | Negative age filters fail closed. |
| `DebtLimitClamped` / `DebtOutputWithinLimit` | Debt finding limits stay in `1..=1000` and bound returned findings. |
| `DebtFindingsProjectScoped` | Debt findings belong to the resolved project id. |
| `DebtFindingsSatisfyFilters` | Returned findings satisfy category, severity, kind, and language filters. |
| `EnrichmentUsesResolvedProject` | Effect enrichment uses the same project id as the primary query. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=768M \
      PGMCP_TLC_METASPACE=32m PGMCP_TLC_CLASS_SPACE=16m \
      PGMCP_TLC_CODE_CACHE=32m \
      ../../../scripts/tlc-capped.sh PredictionReportingScope.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 12 distinct states, 24
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test tool_prediction_integration \
  --test tool_documented_tech_debt --build-jobs 1
```

Result: 8/8 passed. The focused run covers normalized inputs, invalid filter
rejection, JSON output shape, documented marker extraction, severity filtering,
and stale cross-project metric rejection for `code_on_fire`.
