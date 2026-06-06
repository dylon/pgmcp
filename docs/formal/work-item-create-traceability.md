# Work-Item Create Formal Verification Traceability

Status: focused high-use tracker slice for `work_item_create`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`work_item_create` at 12 calls after the higher-volume search, inventory,
graph, memory, fuzzy, telemetry, and lifecycle slices already had focused
coverage.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_create` | Normalize kind/title/public-id/parent/project inputs; reject blank titles and unknown kinds; reject priority/weight values that would trip DB checks; reject unknown projects rather than silently creating global items; keep severity and structured bug fields on `kind='bug'` only; create bugs in `triage` and non-bugs in `pending`; lock a parent before deriving a child root; commit the work-item row and bug-detail sidecar atomically. | `tla/WorkItemCreateAtomicity.tla`; `pgmcp-testing/tests/work_items_smoke.rs`. |

## Issues Found And Corrected

`work_item_create` allowed severity and structured bug sidecar fields on
non-bug items even though `work_items.severity` is documented as bug-only.
Correction: severity and bug-detail fields now require `kind='bug'`.

The tool relied on DB CHECK failures for out-of-range `priority` and
non-positive `weight`, which surfaced as internal errors. Correction: the
tool validates both fields before writing.

An unknown project name previously resolved to `None`, silently creating a
workspace-global item. Correction: create now trims project names and rejects
unknown or ambiguous names.

Bug creation inserted the `work_items` row and `work_item_bug_details` sidecar
in separate transactions. A sidecar failure could leave a partially-created
bug. Correction: create now uses one transaction for the item row and bug
sidecar; failures roll back both.

Child creation previously derived `root_id` in the insert statement without a
parent row lock. Correction: the transaction helper locks the parent row before
deriving the child root.

## Formal Model

`tla/WorkItemCreateAtomicity.tla` models accepted task and bug creates, blank
title rejection, unknown kind rejection, non-bug bug-field rejection, unknown
severity rejection, priority/weight bound failures, unknown project rejection,
duplicate public ids, sidecar failure rollback, and parent-child creation with
a parent lock.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `RejectedWritesNothing` | Invalid requests and DB-side failures leave no item or sidecar row. |
| `BugSidecarAtomic` / `SidecarFailureRollsBack` | Bug item rows and bug-detail sidecars commit or roll back together. |
| `NonBugNeverHasSeverityOrSidecar` / `BugOnlyFieldsRequireBug` | Bug metadata cannot leak onto non-bug work items. |
| `StatusByKind` | Bugs are born in `triage`; non-bugs are born in `pending`. |
| `PriorityAndWeightBounded` | Successful rows satisfy the DB numeric bounds before insertion. |
| `UnknownProjectRejected` / `DuplicatePublicRejected` | Scope typos and duplicate explicit ids fail closed. |
| `ParentLockedBeforeChildInsert` | Child creation locks the parent before deriving the inherited root. |
| `NormalizedCreateFields` | Stored kind, title, and public id are normalized request values. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh WorkItemCreateAtomicity.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 14 distinct states, 28
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke work_item_tracker_full_round_trip --build-jobs 1
```

Result: 1/1 passed. The focused run covers normalized create input, priority
and weight rejection, unknown project rejection, bug-field rejection on
non-bugs, root/child creation, and the existing tracker round trip.
