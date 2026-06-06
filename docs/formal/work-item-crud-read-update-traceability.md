# Work-Item CRUD Read/Update Formal Verification Traceability

Status: focused high-use tracker slice for `work_item_get`, `work_item_list`,
and `work_item_update`.

## Scope

The 31-day telemetry snapshot showed `work_item_list` and `work_item_update`
at 10 calls each after `work_item_create`, `work_item_set_status`, and
`work_item_record_progress` were already covered. `work_item_get` is covered
with them because it shares the public-id lookup boundary.

Local correctness obligations:

| Tools | Obligations | Evidence |
| --- | --- | --- |
| `work_item_get`, `work_item_list`, `work_item_update` | Trim/reject public ids; validate list project/kind/status filters; clamp list limits; reject blank update titles; validate update priority/weight before DB checks; keep severity and structured bug fields on bugs only; commit mutable field updates and bug sidecar writes atomically. | `tla/WorkItemCrudReadUpdate.tla`; `pgmcp-testing/tests/work_items_smoke.rs`. |

## Issues Found And Corrected

Public-id consumers used raw strings. Correction: the shared `id_of_public`
path now trims and rejects blank ids; `work_item_get` uses the same boundary.

`work_item_list` used permissive project resolution where an unknown project
name became a global query. It also accepted raw kind/status filters.
Correction: list now trims project/kind/status filters, rejects unknown
projects/kinds/statuses, and still relies on the query layer's `1..=1000`
limit clamp.

`work_item_update` could set a blank title, relied on DB CHECKs for
out-of-range priority/weight, and allowed bug metadata on non-bug items.
Correction: update rejects blank titles, invalid priority/weight, and bug-only
fields on non-bugs before persistence.

`work_item_update` wrote mutable row fields and bug-detail sidecar fields in
separate transactions. Correction: the row update and sidecar write now use a
single transaction and roll back together.

## Formal Model

`tla/WorkItemCrudReadUpdate.tla` models get lookup normalization, blank/missing
public ids, list filter validation, limit clamping, successful updates,
blank-title/priority/weight failures, bug-field rejection on non-bugs, and a
sidecar failure after a bug update would otherwise have happened.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankPublicRejected` / `NormalizedPublicStored` | Public ids are trimmed and blanks are rejected. |
| `ListFiltersValidated` / `UnknownListFiltersFailClosed` | List filters cannot typo into global or unvalidated queries. |
| `ListLimitClamped` | List limits stay in the query layer's bounded range. |
| `UpdateBoundsChecked` | Successful updates satisfy DB numeric bounds before write. |
| `BugFieldsOnlyOnBugs` | Bug metadata cannot be updated on non-bug items. |
| `UpdateAndSidecarAtomic` / `SidecarFailureRollsBackUpdate` | Mutable row changes and bug sidecar changes commit or roll back together. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh WorkItemCrudReadUpdate.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 14 distinct states, 28
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --test work_items_bugs_smoke --test work_items_tags_progress_smoke --build-jobs 1
```

Result: 9/9 passed. The focused run covers normalized get/update/list inputs,
invalid update/list rejection cases, bug-only update rejection, and the broader
tracker smoke paths.
