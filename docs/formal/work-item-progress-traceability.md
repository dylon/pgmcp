# Work-Item Progress Formal Verification Traceability

Status: focused high-use-tool slice for `work_item_record_progress`, the first
tracker tool after the search/inventory/config tools in the 31-day telemetry
ranking.

## Scope

The durable telemetry snapshot used for this slice showed
`work_item_record_progress` at 52 calls. The tool is intentionally agent-grade:
it records useful activity and self-reported percent, but it cannot create
trusted completion evidence.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_record_progress` | Reject empty notes; clamp self-reported percent into `[0, 100]`; always write MCP-authored rows with `provenance='agent_write'`; update only `claimed_percent`, not verified roll-up state. | `tla/WorkItemProgressLog.tla`; `pgmcp-testing/tests/work_items_tags_progress_smoke.rs`. |
| `work_item_progress_log` | Return the append-only progress log newest-first and reject missing items through the public-id lookup boundary. | `pgmcp-testing/tests/work_items_tags_progress_smoke.rs`. |

## Formal Model

`tla/WorkItemProgressLog.tla` models a finite set of MCP progress requests:
empty note, in-range percent, high percent, low percent, and no-percent update.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `NoEmptyNoteRecorded` / `EmptyRequestNeverRecorded` | Empty progress notes are rejected, not logged. |
| `McpProgressAlwaysAgentWrite` | MCP-authored progress cannot claim trusted/user provenance. |
| `ProgressPercentClamped` / `ClaimedPercentClamped` | Stored progress percentages and the item's `claimed_percent` stay in the clamped domain. |
| `ProgressDoesNotVerify` | Recording progress never increments verified roll-up state. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-WorkItemProgressLog \
  -config docs/formal/tla/WorkItemProgressLog.cfg \
  docs/formal/tla/WorkItemProgressLog.tla
```

Result: 516 distinct states, 580 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_tags_progress_smoke --build-jobs 1
```

Result: 1/1 passed. The smoke covers `agent_write` provenance, empty-note
rejection, newest-first progress log ordering, `250 -> 100` high-percent clamp,
`-5 -> 0` low-percent clamp, and `claimed_percent` following the latest
percent-bearing agent note.
