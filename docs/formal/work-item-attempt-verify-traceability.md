# Work Item Attempt Verify Formal Verification Traceability

Status: focused tracker gatekeeper slice for `work_item_attempt_verify`.

## Scope

`work_item_attempt_verify` is the MCP-facing gatekeeper path from a claimed
item to `verified`. It must reject manual-only evidence, preserve status on
failure, and only count completed verifications after a successful trusted
evidence transition.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_attempt_verify` | Resolve the target item; require a gatekeeper-valid source/status/evidence combination; reject missing items, wrong statuses, absent evidence, manual-only evidence, and trusted failures without status mutation; publish `verified` only with sufficient trusted passing evidence; increment the verification counter only after the verified transition. | `tla/WorkItemAttemptVerifyGate.tla`; `work_items_smoke` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool correctly used `Actor::Gatekeeper`, which delegates the hard trust
decision to `set_work_item_status`, but `work_item_verifications` was incremented
before that transition succeeded. Failed manual-evidence attempts therefore
advanced the completed-verification counter.

The counter now increments only after `set_work_item_status` returns the updated
verified row.

## Formal Model

`tla/WorkItemAttemptVerifyGate.tla` models successful trusted evidence and the
main rejection classes: missing item, wrong status, no evidence, manual-only
evidence, and trusted failing evidence.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `RejectedLeavesStatusAndStats` | Rejected verification attempts leave status and verification counters unchanged. |
| `ManualEvidenceCannotVerify` | Manual-only evidence cannot publish `verified`. |
| `TrustedEvidenceRequired` | Any `verified` result has trusted evidence. |
| `CounterAfterVerifiedTransition` | The verification counter increments only after a verified transition. |
| `OnlyTrustedPassVerifies` | No modeled rejection class can verify work. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkItemAttemptVerifyGate.tla
```

Result: 7 distinct states, 13 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: pending until sibling dependency compilation is restored.
