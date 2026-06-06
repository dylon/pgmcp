# Session Mandate Promotion Formal Verification Traceability

Status: focused session-mandate read/promotion slice for `session_mandates` and
`promote_session_mandate`.

## Scope

The 31-day `mcp_tool_calls` ranking left `promote_session_mandate` in the next
uncovered group. Its companion read tool shares the same module and trust
boundary, so this slice covers both surfaces.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `session_mandates` | Normalize and validate status filters; require a nonblank `session_id` or `cwd`; clamp result limits; keep `cwd` scoping for every status filter, including `all`. | `tla/SessionMandatePromotion.tla`; `oracle_session_mandates`. |
| `promote_session_mandate` | Normalize/validate scope, project id, and target file before mutation; reject ineligible mandates; serialize promotion under the source-row lock; return the existing durable row on repeat/concurrent promotion; use a single non-blocking advisory file lock for optional appends; release locks before return. | `tla/SessionMandatePromotion.tla`; `oracle_session_mandates`. |

## Issues Found And Corrected

`session_mandates` used the scoped helper for default active reads, but its
non-active follow-up query ignored `cwd`. A caller asking for `status='all'`
with only `cwd` could receive rows from unrelated sessions.

Correction: status filters are closed and normalized, blank selectors fail
closed, limits are clamped once, and non-active queries now use the same
session-id-or-cwd scoping rule as active reads.

`promote_session_mandate` accepted raw scope/project/file values and repeat
promotion could insert another `durable_mandates` row for the same source
mandate. Concurrent callers were serialized by the source row lock, but the
transaction did not check for an existing durable row after acquiring that
lock.

Correction: the tool validates inputs before mutation, and
`sessions::promote_mandate` checks for an existing durable row inside the
`SELECT ... FOR UPDATE` transaction. Matching repeats return the existing id;
mismatched scope/project/target requests fail closed.

The optional file append was a read-modify-write with no inter-call exclusion.
Concurrent appends to the same target could lose one update.

Correction: `write_to_file=true` acquires one PostgreSQL advisory file lock
with `pg_try_advisory_lock` before DB promotion. A busy file fails before DB
writes, successful appends release the lock before return, and the append
remains idempotent on retry.

## Formal Model

`tla/SessionMandatePromotion.tla` models scoped reads, invalid statuses,
missing selectors, new promotion, repeated promotion, mismatched repeat
promotion, invalid project/scope/file inputs, inactive source rows, and
busy/free file-lock paths.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidReadsReject` | Missing selectors and invalid statuses return no rows. |
| `CwdReadRowsStayScoped` | `cwd` reads, including `status='all'`, surface only rows for that `cwd`. |
| `InvalidPromotionsDoNotWrite` | Invalid or busy promotion requests do not mutate durable DB/file state. |
| `RepeatedPromotionIsIdempotent` | Replays return one existing durable mandate without inserting another. |
| `NewPromotionCreatesOneDurable` | A fresh valid promotion creates exactly one durable mandate. |
| `FileLockIsNonBlocking` | Busy target-file paths reject instead of blocking or writing. |
| `NoHeldLocksAtReturn` | The model has no path that returns while holding a lock. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_session_mandates --build-jobs 1
```

Result: 8/8 passed for the focused session-mandate oracle suite.

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh SessionMandatePromotion.tla
```

Result: exit 0; no invariant violations; 12 distinct states and 24 states
generated at depth 1.
