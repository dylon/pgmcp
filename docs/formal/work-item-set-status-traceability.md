# Work-Item Set-Status Formal Verification Traceability

Status: focused high-use tracker slice for `work_item_set_status`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`work_item_set_status` at 14 calls. The tool is deliberately agent-grade:
agents may move work through normal execution states, but cannot judge their
own work as verified, rejected, or deferred.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_set_status` | Reject blank public ids and statuses; normalize public id, status, and reason strings; reject unknown statuses; always act as `Actor::Agent`; refuse agent attempts to write judgment states; validate the transition against the row-locked current status; write the status update and status-history row atomically. | `tla/WorkItemSetStatusAtomicity.tla`; `pgmcp-testing/tests/work_items_smoke.rs`. |

## Issues Found And Corrected

`set_work_item_status` loaded the current item status and transition evidence
before opening its transaction. Two concurrent requests could both validate
from the same stale status, then both update and insert misleading history rows.

Correction: the query now begins a transaction first, selects the item row
`FOR UPDATE`, reads the evidence context inside that transaction, runs
`check_transition` against the locked status, and writes the item update plus
history row in the same transaction. Any failure rolls the transaction back.

The MCP tool also used raw request strings. Correction: `public_id` and
`status` are trimmed and rejected if empty; `reason` is trimmed and
whitespace-only reasons are omitted before history insertion.

## Formal Model

`tla/WorkItemSetStatusAtomicity.tla` models blank/whitespace public ids,
blank statuses, unknown statuses, a normalized successful transition, forbidden
agent judgment transitions, and two race orders between `pending -> triage` and
`pending -> in_progress`.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankPublicRejected` / `BlankStatusRejected` / `UnknownStatusRejected` | Invalid requests do not mutate item status or status history. |
| `HistoryRowsUseAgentActor` | MCP-authored status-history rows are always agent rows. |
| `HistoryRowsAreLegalAgentTransitions` | Every committed history row satisfies the agent transition matrix. |
| `AgentNeverWritesJudgmentStatus` | MCP status changes cannot target `verified`, `rejected`, or `deferred`. |
| `HistoryAtomicWithItemStatus` | A committed status change and its history row agree atomically. |
| `RaceRequestsSerialize` / `AtMostOnePendingTransitionCommits` | Racing requests recheck after the first commit; only one transition observes `pending`. |
| `StoredReasonsAreNormalized` | Persisted history reasons are trimmed or omitted. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh WorkItemSetStatusAtomicity.tla)
```

Result: 21 distinct states, 31 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: 7/7 passed. The focused suite covers the full tracker round trip,
agent self-verify refusal, normalized status/reason handling, and the
concurrent `pending -> triage` versus `pending -> in_progress` regression.
