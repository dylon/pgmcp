# Work Item Defer/Reinstate Formal Verification Traceability

Status: focused user-authority slice for `work_item_defer` and
`work_item_reinstate`.

## Scope

Defer and reinstate are user-token operations. They must be unreachable to
agents, and their scope-negotiation audit rows must not become visible unless
the paired status transition also commits.

Local correctness obligations:

| Tools | Obligations | Evidence |
| --- | --- | --- |
| `work_item_defer`, `work_item_reinstate` | Validate the user token before opening a transaction; reject blank reasons before writes; commit scope-negotiation rows and status-history/status updates atomically; roll back negotiation rows if the transition is illegal; increment status-change counters only after commit. | `tla/WorkItemDeferReinstateAtomicity.tla`; `work_items_smoke` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tools incremented `work_item_status_changes` before token validation and
before a status transition could succeed. More importantly, `work_item_defer`
inserted a `scope_negotiations` row before calling `set_work_item_status`; if
the transition failed, the negotiation row could remain without a matching
status transition.

The query layer now exposes transactional variants for status transitions and
scope-negotiation inserts. Defer and reinstate use one transaction for the
negotiation row and status transition, then increment the status-change counter
only after commit.

## Formal Model

`tla/WorkItemDeferReinstateAtomicity.tla` models valid defer/reinstate calls,
bad token, blank reason, missing item, and a transition failure after the
transaction starts.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `UserTokenRequiredBeforeTx` | Bad tokens reject before transaction or writes. |
| `LocalRejectsWriteNothing` | Local rejects create no negotiation/status rows and do not increment counters. |
| `TransitionFailureRollsBackNegotiation` | Transition failure after transaction start rolls back the negotiation row. |
| `SuccessfulDeferAtomic` | A successful defer commits negotiation and status-history rows together. |
| `SuccessfulReinstateAtomic` | A successful reinstate commits negotiation and status-history rows together. |
| `CounterAfterCommittedStatusChange` | The status-change counter increments only after a committed status change. |
| `NoOrphanNegotiation` | No negotiation row is visible without a committed status-history row. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkItemDeferReinstateAtomicity.tla
```

Result: 7 distinct states, 13 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: pending until sibling dependency compilation is restored.
