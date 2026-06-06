# Experiment Decide Formal Verification Traceability

Status: focused high-use experiment slice for `experiment_decide`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed `experiment_decide` at 5 calls.
Unlike read-only reporting tools, this is a trust-boundary write: it persists a
statistical decision and publishes the hypothesis verdict plus experiment
status.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_decide` | Reject invalid hypothesis ids and blank/equal metric or arm labels before writing; bound operator-controlled text; keep the anti-p-hacking criterion-time guard; persist result, hypothesis verdict, experiment status, and observation pointers atomically; leave best-effort mirrors/work-item/outcome bridges outside the core decision transaction. | `tla/ExperimentDecideAtomicity.tla`; `tool_experiments_integration`. |

## Issues Found And Corrected

The tool inserted an `experiment_results` row and then updated the hypothesis
verdict and experiment status with separate statements. A database failure
between those writes could leave a visible decision row without the corresponding
published verdict/status.

Correction: `insert_experiment_decision` now wraps result insertion,
hypothesis-verdict publication, experiment-status publication, and observation
pointer updates in one SQL transaction.

The request boundary did not normalize optional metric/control/treatment labels
or reject equal arm labels. Those inputs are now trimmed, length-bounded, and
validated before samples are loaded or any writes occur.

## Formal Model

`tla/ExperimentDecideAtomicity.tla` models valid and invalid decide requests plus
a database-failure branch inside the publication step.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsNoWrite` | Invalid request shapes never append decision rows. |
| `DbFailureRollsBack` | A failed publication attempt reports no committed write. |
| `CommittedDecisionAtomic` | A committed response implies result, verdict, and status are all visible together. |
| `NoPartialPublishedState` | The model cannot reach a state with only part of the result/verdict/status triple published. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh ExperimentDecideAtomicity.tla
```

Result: exit 0; no invariant violations; 227 distinct states and 308 states
generated at depth 9.

```bash
cargo nextest run -p pgmcp-testing --test tool_experiments_integration --build-jobs 1
```

Result: 2/2 passed, including the full accepted-decision round trip and invalid
same-arm no-write regression.
