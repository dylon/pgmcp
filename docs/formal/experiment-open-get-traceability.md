# Experiment Open/Get Formal Verification Traceability

Status: focused high-use experiment lifecycle slice for `experiment_open` and
`experiment_get`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed `experiment_open` at 12 calls
with 4 errors and `experiment_get` at 12 calls with 1 error. This slice follows
the higher-volume experiment artifact and measurement slices.

Local correctness obligations:

| Tools | Obligations | Evidence |
| --- | --- | --- |
| `experiment_open`, `experiment_get` | Normalize open fields and slug lookups; reject blank required open fields; reject unknown kinds, predicted directions, and project ids before enum/FK casts; commit the experiment row and first hypothesis atomically; require a positive `experiment_id` or nonblank slug for get; trim get slugs; preserve id-over-slug precedence; never return a successfully opened experiment without its hypothesis. | `tla/ExperimentOpenGetAtomicity.tla`; `pgmcp-testing/tests/tool_experiments_integration.rs`. |

## Issues Found And Corrected

`experiment_open` validated required strings with `trim()` but persisted the
raw strings. It also passed raw `kind`, `predicted_direction`, and `slug`
through to protocol generation and enum casts. Correction: open now uses
trimmed title/question/hypothesis/metric values, trims explicit slug/kind/
direction, defaults blank kind/direction, and rejects unknown enum values
before the DB.

`experiment_open` inserted the experiment and first hypothesis in separate
transactions. A hypothesis insert failure could leave a partial experiment row
that `experiment_get` could later return with no hypotheses. Correction: the
core experiment row and first hypothesis now commit in one transaction. Code
anchors and memory mirroring remain best-effort after the core record commits.

Unknown `project_id` values previously surfaced as FK/internal errors.
Correction: open validates positive project ids and rejects unknown ids before
insertion.

`experiment_get` accepted neither id nor slug and reported a generic not-found
error; padded slugs failed lookup. Correction: get now requires a positive id
or nonblank slug and trims slug lookups while preserving id-over-slug
precedence.

## Formal Model

`tla/ExperimentOpenGetAtomicity.tla` models successful padded open, blank
required fields, unknown kind/direction/project, a hypothesis insertion
failure after the experiment insert would otherwise have happened, missing and
bad get identifiers, trimmed slug lookup, missing slug lookup, and id-over-slug
precedence.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `OpenCommitsExperimentAndHypothesisTogether` | Open never leaves only one of the core row or hypothesis committed. |
| `HypothesisFailureRollsBackExperiment` | A hypothesis failure rolls back the experiment insert. |
| `OpenFieldsNormalized` | Successful open stores normalized slug/kind/direction values. |
| `InvalidOpenRejectedBeforeWrite` | Invalid open requests do not persist rows. |
| `GetRequiresLookup` / `GetRejectsBadId` | Get requires an explicit valid lookup key. |
| `GetSlugTrimmed` | Whitespace-padded slugs resolve to the normalized slug. |
| `GetIdWinsOverSlug` | A supplied id remains authoritative when slug is also supplied. |
| `GetOkHasHypothesis` | Successful get results are not partial opens. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh ExperimentOpenGetAtomicity.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 12 distinct states, 24
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test tool_experiments_integration --build-jobs 1
```

Result: 2/2 passed. The focused run covers normalized open input, invalid
direction/project rejection, get lookup validation, trimmed slug lookup, and
the full experiment lifecycle.
