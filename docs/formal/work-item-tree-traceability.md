# Work Item Tree Formal Verification Traceability

Status: focused tracker read slice for `work_item_tree`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `work_item_tree` at 3 calls.
The tool resolves one `public_id` and returns the materialized descendant tree
for a plan or work-item subtree.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_tree` | Reject blank or missing roots; clamp `max_rows` to a finite range; always include the root for valid reads; bound output by the effective limit; order rows deterministically by depth, priority, then id; suppress corrupted parent cycles; return duplicate-free rows; remain read-only; increment work-item query stats on successful tree reads. | `tla/WorkItemTreeScope.tla`; `oracle_work_item_tree`; filtered `work_items_smoke`. |

## Issues Found And Corrected

The recursive CTE relied on a final `LIMIT` but did not track visited nodes. A
corrupted parent cycle could therefore keep producing recursive rows before the
`ORDER BY depth, priority DESC, id` step materialized the result.

Correction: recursive traversal now carries a `path` array of visited work-item
ids, refuses to revisit an id already on the path, and caps path depth by the
effective row limit.

`work_item_tree` also did not increment the tracker read-query counter, unlike
the neighboring read tools.

Correction: successful tree reads now increment `work_item_queries`.

## Formal Model

`tla/WorkItemTreeScope.tla` models public-id validation, row-limit clamping,
root inclusion, depth/priority ordering, cycle suppression, duplicate-free rows,
read-only behavior, and query-stat accounting.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidPublicIdRejects` | Blank or missing roots reject without subtree rows or query-stat increments. |
| `LimitClamped` | Effective row limits stay in `1..=100000`. |
| `RowsBoundedByLimit` | Returned row count never exceeds the effective limit. |
| `RootIncludedWhenValid` | A valid tree read includes the requested root. |
| `DepthPriorityOrdering` | Same-depth children are ordered by priority descending. |
| `CycleSuppressedFinite` | Corrupted cycles are suppressed and remain finite. |
| `NoDuplicateRows` | Tree output does not repeat cycle nodes. |
| `ReadOnlyNoLock` | The tool has no write or held-lock path. |
| `StatsIncrementOnlyOnSuccessfulTreeRead` | Query stats increment only after a successful tree read. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_work_item_tree --build-jobs 1
```

Result: 2/2 passed for limit/order behavior and corrupted-cycle suppression.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke \
  work_item_tracker_full_round_trip --build-jobs 1
```

Result: 1/1 passed for the existing tracker round-trip path including
`work_item_tree`.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh WorkItemTreeScope.tla
```

Result: TLC exit 0; 6 distinct states, 12 states generated; no invariant
violations.
