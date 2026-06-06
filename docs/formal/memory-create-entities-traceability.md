# Memory Create Entities Formal Verification Traceability

Status: focused high-use memory-write slice for `memory_create_entities`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`memory_create_entities` in the same low-call high-risk cluster as the
architecture tools. The tool is a write boundary, so the verification target is
stronger than request scoping: invalid requests must be no-write, active entity
identity must be race-safe, and observation insertion must remain idempotent.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_create_entities` | Validate and bound create batches before scope creation; normalize entity names/types; fail closed on pre-existing duplicate active identities; serialize concurrent same-identity creates without Rust mutexes; acquire all advisory locks in a deterministic order; attach duplicate observations at most once; report actual inserted entity/observation counts. | `tla/MemoryCreateEntitiesAtomicity.tla`; `memory_phase2_3`. |

## Issues Found And Corrected

The DB query used a select-then-insert sequence for active
`(name, entity_type)` rows:

```sql
SELECT id FROM memory_entities
WHERE name = $1 AND entity_type = $2 AND valid_to IS NULL
LIMIT 1
```

Because the schema's uniqueness includes `valid_from`, concurrent callers could
both observe no active row and insert duplicate active entities.

Correction: `memory_create_entities_detailed` now takes transaction-scoped
Postgres advisory locks keyed by the normalized entity identity. It acquires
the unique identity keys in sorted order before any entity insert. Observation
locks are then acquired by sorted entity id before inserting observations, so
batch calls cannot form a lock-order cycle.

If old data already contains multiple active rows for the same normalized
identity, the query now rejects the request instead of selecting an arbitrary
row.

The wrapper also used to call `find_or_create_scope` before validating the
entity payload. That let invalid requests create a scope row. Validation now
happens before scope resolution, so invalid batches are no-write.

Observation idempotence previously relied on `ON CONFLICT` against
`(entity_id, content_sha256, valid_from)`, which does not dedupe repeated active
observations across separate transactions. The shared insert helper now checks
for an active `(entity_id, content_sha256)` row under a per-entity observation
lock before inserting.

## Formal Model

`tla/MemoryCreateEntitiesAtomicity.tla` models two create transactions that ask
for the same entities in opposite raw orders plus an observation-only
transaction. The implementation's sorted lock order is represented as identity
locks before observation locks.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `LockOrderSorted` | Every transaction acquires locks in the implementation's total order. |
| `NoLockOrderInversion` | A waiting transaction never waits for a lower-ranked lock while holding a higher-ranked one. |
| `NoDuplicateActiveEntityCreates` | At most one transaction inserts a given active entity identity. |
| `NoDuplicateObservationInserts` | At most one transaction inserts a given active observation content for an entity. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh MemoryCreateEntitiesAtomicity.tla
```

Result: 68 distinct states, 98 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase2_3 --build-jobs 1
```

Result: 13/13 passed, including invalid no-write, normalized reuse,
pre-existing duplicate identity rejection, active-observation dedupe, and
concurrent same-identity create regressions.
