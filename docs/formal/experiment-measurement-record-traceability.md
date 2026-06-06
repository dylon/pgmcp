# Experiment Measurement Formal Verification Traceability

Status: focused high-use experiment slice for `experiment_record_measurement`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`experiment_record_measurement` at 15 calls. The tool accepts raw measurement
samples for an experiment arm and is part of the experiment subsystem's
evidence chain.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_record_measurement` | Reject malformed measurement inputs before writing; validate that a supplied hypothesis belongs to the supplied experiment; bound per-call sample insertion; normalize arm label, metric, source, and unit keys; commit run/sample/status changes atomically; serialize run upserts without nested locks; exclude warm-up samples from conformance counts. | `tla/ExperimentMeasurementRecord.tla`; `pgmcp-testing/tests/tool_experiments_integration.rs`. |

## Issues Found And Corrected

The tool only validated non-empty finite samples, arm kind, and unit-key length.
It accepted empty arm labels, empty metric names, arbitrary `source` strings,
empty or duplicate unit keys, and hypothesis ids from other experiments.

Correction: the MCP boundary now normalizes and validates those fields before
any write. It rejects missing experiments, missing hypotheses, and
experiment/hypothesis mismatches with `invalid_params`.

The write path performed run upsert, sample insert, and experiment-status update
as separate DB operations. A mid-write failure could leave partial state.

Correction: `record_experiment_measurement` now wraps the run row, sample rows,
and status update in one transaction. Any DB error rolls back the whole
measurement write.

Concurrent NULL-hypothesis run upserts were not protected by the table-level
unique constraint because Postgres treats `NULL` values as distinct.

Correction: the transaction takes exactly one `pg_advisory_xact_lock` keyed by
the run identity before the null-safe lookup/upsert. Because the transaction
takes only one advisory lock, there is no lock-order cycle in this path.

The capped TLC wrapper's JVM defaults still reserved too much class/metaspace
under the fallback address-space cap. Correction: the defaults are now a 512 MiB
heap, 64 MiB metaspace, and 16 MiB compressed class space.

## Formal Model

`tla/ExperimentMeasurementRecord.tla` models invalid labels, metrics, sample
counts, non-finite samples, invalid sources, bad unit keys, missing experiments,
missing/foreign hypotheses, normalized successful requests, DB rollback, and
warm-up submissions.

The spec is one-shot: it picks an arbitrary request and follows that call
through validation, advisory-lock acquisition, commit, rollback, or rejection.
This preserves the per-call correctness obligations without exploring
irrelevant request-order permutations.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsHaveNoSamplesOrStatus` | Invalid requests do not write samples or status updates. |
| `DbFailuresRollback` | Modeled DB failures leave no committed measurement side effects. |
| `AtomicRunSamplesStatus` | A successful write has run, samples, and status as one atomic unit. |
| `CommittedRowsAreValidated` | Every committed batch came from a valid, non-failing request. |
| `CommittedTextIsNormalized` | Committed arm, metric, and source values are normalized. |
| `SampleCountsAreBounded` | Committed sample batches are non-empty and within the modeled cap. |
| `WarmupsExcludedFromConformance` | Warm-up rows are retained but contribute zero conformance samples. |
| `NoDanglingLock` | The model never leaves a lock held outside the locked transaction state. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh ExperimentMeasurementRecord.tla)
```

Result: 35 distinct states, 50 generated states, no invariant violations.

```bash
cargo check -p pgmcp-testing --test tool_experiments_integration
```

Result: passed. The focused integration coverage rejects empty arm labels,
duplicate normalized unit keys, invalid sources, and foreign hypotheses, then
verifies a normalized accepted submission reaches `experiment_runs` and
`experiment_samples`.
