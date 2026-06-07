# Deadlock Analysis Formal Verification Traceability

Status: concurrency/deadlock slice for `deadlock_cycles`, `lock_order_graph`,
and `channel_deadlock`.

## Scope

The operational deadlock arguments already exist:

- `tla/LockOrderDeadlock.tla` and `rocq/LockOrderDeadlock.v` cover the
  lock-order analysis behind `deadlock_cycles` and `lock_order_graph`.
- `tla/ChannelDeadlock.tla` and `rocq/ChannelDeadlock.v` cover the Petri-net
  channel-deadlock analysis behind `channel_deadlock`.

This slice adds the MCP wrapper obligations around those algorithms.

| Tool | Contract | Evidence |
| --- | --- | --- |
| `deadlock_cycles` | Resolve one non-ambiguous project; reject non-finite confidence floors; clamp confidence/depth/cycle/output bounds; query only resolved-project sync skeleton and call edges; return bounded cycles; remain read-only with no held locks. | `tla/DeadlockAnalysisBoundary.tla`; `tla/LockOrderDeadlock.tla`; `rocq/LockOrderDeadlock.v`; filtered `tool_concurrency_deadlock` once sibling dependency compilation is restored. |
| `lock_order_graph` | Share the same finite confidence/depth normalization and resolved-project lock-order graph as `deadlock_cycles`; expose a read-only graph view. | `tla/DeadlockAnalysisBoundary.tla`; `tla/LockOrderDeadlock.tla`; `rocq/LockOrderDeadlock.v`; filtered `tool_concurrency_deadlock` once sibling dependency compilation is restored. |
| `channel_deadlock` | Resolve one non-ambiguous project; clamp output bounds; analyze only resolved-project message skeleton rows; surface bounded channel findings; remain read-only with no held locks. | `tla/DeadlockAnalysisBoundary.tla`; `tla/ChannelDeadlock.tla`; `rocq/ChannelDeadlock.v`; filtered `tool_concurrency_deadlock` once sibling dependency compilation is restored. |

## Issue Found And Corrected

`deadlock_cycles` and `lock_order_graph` accepted `confidence_floor` through
`f32::clamp`. A non-finite value can pass through ordinary comparisons as NaN,
which would make the confidence filter unsound. The wrappers now reject
non-finite confidence floors before analysis, then clamp finite values to
`0.0..=1.0`. Responses also include the normalized project and effective
parameters so tests can pin the boundary.

## Model

`tla/DeadlockAnalysisBoundary.tla` models:

- blank, duplicate, trimmed, and valid project names;
- finite low/high/default confidence values plus an explicit non-finite token;
- oversized and undersized depth, cycle-length, and output limits;
- lock and channel rows split across two projects;
- read-only execution with no retained locks.

Key invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidNoScan` | Invalid project or non-finite confidence requests reject before scans. |
| `FiniteConfidenceOnly` | Successful lock-order calls use finite normalized confidence. |
| `BoundedDeadlockParams` | `deadlock_cycles` uses finite bounded depth, cycle length, and output limits. |
| `BoundedChannelParams` | `channel_deadlock` uses a finite bounded output limit. |
| `RowsScoped` | Lock/channel rows in successful responses belong to the resolved project. |
| `ReadOnlyAndNoLocksHeld` | The wrappers do not write and do not retain locks after returning. |

## Verification Run 2026-06-07

TLC, using the RSS-capped wrapper:

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh DeadlockAnalysisBoundary.tla
```

Result: 13 distinct states, 19 generated states, no invariant violations.

Rust:

```bash
cargo nextest run -p pgmcp-testing --test tool_concurrency_deadlock \
  --build-jobs 1
```

Result: blocked before pgmcp tests by sibling `libgrammstein` compile errors
(`PersistentARTrieChar::read` trait import and `Option<i64>::copied()`).
