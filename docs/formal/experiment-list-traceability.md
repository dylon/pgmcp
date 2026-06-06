# Experiment List Formal Verification Traceability

Status: focused experiment read/list slice for `experiment_list`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `experiment_list` at 2
calls. The tool lists active experiments, optionally filtered by project id,
experiment kind, and status, sorted newest-first with limit/offset pagination.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_list` | Reject invalid enum filters and non-positive project ids; trim valid kind/status filters; omit blank filters; clamp pagination; apply filtering and `LIMIT/OFFSET` in SQL; preserve newest-first ordering; remain read-only with no persistent locks. | `tla/ExperimentListScope.tla`; `oracle_experiment_list`; filtered `tool_experiments_integration`. |

## Issues Found And Corrected

`experiment_list` accepted raw `kind` and `status` strings and compared them to
`e.kind::text` / `e.status::text`. Whitespace-padded valid filters returned
empty pages, invalid filters silently returned empty pages, and casting the
indexed enum columns to text made the SQL less faithful to the schema's closed
vocabulary.

Correction: the list tool now reuses the experiment-kind normalizer, adds a
status normalizer, rejects non-positive `project_id`, trims valid filters, and
treats blank filters as omitted. The query compares enum columns directly via
`$2::experiment_kind` and `$3::experiment_status`.

The old response did not report effective `limit`/`offset` or filters, which
made it hard for callers and tests to confirm the bounded page actually used.

Correction: the response now includes `limit`, `offset`, and a `filters` object
with the normalized filter values.

## Formal Model

`tla/ExperimentListScope.tla` models valid/invalid project, kind, and status
filters; blank-filter omission; bounded limit/offset normalization; bounded
returned rows; project-scope preservation; newest-first page semantics; and
read-only execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidFiltersReject` | Invalid enum filters or non-positive project ids reject before returning rows. |
| `BlankFiltersOmitted` | Blank optional filters become omitted filters. |
| `TrimmedFiltersAccepted` | Whitespace-padded valid enum filters reach the ok path. |
| `LimitAndOffsetBounded` | Effective limit stays in `1..=500` and offset is non-negative. |
| `ReturnedRowsBounded` | Materialized rows never exceed the effective page limit. |
| `ProjectScopeHonored` | A project-scoped request cannot return another project's row. |
| `NewestFirstPagination` | Accepted pages preserve newest-first ordering semantics. |
| `ReadOnlyNoLock` | The tool has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_experiment_list --build-jobs 1
```

Result: 3/3 passed for normalized filters/bounds/read-only behavior, invalid
filter rejection, and newest-first pagination with offset.

```bash
cargo nextest run -p pgmcp-testing --test tool_experiments_integration \
  experiment_subsystem_full_round_trip --build-jobs 1
```

Result: 1/1 passed for the existing experiment subsystem round trip, including
the `experiment_list` call path.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh ExperimentListScope.tla
```

Result: TLC exit 0; 7 distinct states, 14 states generated; no invariant
violations.
