# Memory Create Relations Formal Verification Traceability

Status: focused memory-write slice for `memory_create_relations`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `memory_create_relations` in the
next 2-call cluster after `experiment_render_ledger`. The tool is an
official-compatible memory graph write boundary, so the verification target is
stronger than response shape compatibility: invalid requests must be no-write,
ambiguous endpoints must fail closed, repeated active relation creates must be
idempotent, and concurrent same-triple creates must not produce duplicate active
rows.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_create_relations` | Bound and normalize relation fields before DB writes; reject blank/overlong fields; fail closed on ambiguous active endpoint names; treat missing endpoints and self-loops as unresolved no-writes; serialize concurrent same-triple creates with transaction-scoped advisory locks; acquire all relation locks in deterministic sorted order; report actual inserted relation count separately from resolved IDs. | `tla/MemoryCreateRelationsAtomicity.tla`; `memory_phase2_3`. |

## Issues Found And Corrected

The DB query resolved endpoint names using `LIMIT 1`:

```sql
SELECT id FROM memory_entities
WHERE name = $1 AND valid_to IS NULL
LIMIT 1
```

If old data contained multiple active entities with the same name, relation
creation could attach to an arbitrary endpoint. The query now loads all active
endpoint IDs in deterministic order and rejects ambiguous names instead of
choosing one.

The query also used a select-then-insert sequence for active relation triples.
Because the schema uniqueness includes `valid_from`, concurrent callers could
both observe no active row and insert duplicate active relation rows. The
correction mirrors the proven entity-create pattern: normalize relation triples,
dedupe and sort the advisory-lock keys, acquire transaction-scoped Postgres
locks in that total order, then re-check active relation rows before inserting.

The MCP wrapper accepted unbounded raw strings and counted every resolved
relation ID as `relations_created`, including idempotent reuses. The wrapper now
rejects empty batches, caps relation batches at 500, trims endpoint/type fields,
rejects blank or over-256-byte fields, and reports actual inserts as
`relations_created` while exposing `relations_resolved` for compatibility
visibility.

## Formal Model

`tla/MemoryCreateRelationsAtomicity.tla` models two valid create transactions
that ask for the same two relation triples in opposite raw orders, plus
invalid, missing-endpoint, self-loop, and ambiguous-endpoint no-write requests.
The implementation's sorted lock order is represented as a total order over the
relation advisory-lock keys.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `LockOrderSorted` | Every valid transaction acquires relation locks in the implementation's total order. |
| `NoLockOrderInversion` | A waiting transaction never waits for a lower-ranked relation lock while holding a higher-ranked one. |
| `NoDuplicateActiveRelationCreates` | At most one transaction inserts a given active relation triple. |
| `NoInvalidOrUnresolvedWrites` | Invalid, ambiguous, missing-endpoint, and self-loop requests insert no relation rows. |
| `NoWriteWithoutResolvedEndpoints` | Only fully resolved valid create transactions can insert. |
| `InsertedRowsAreActive` | Any inserted relation is present in the active relation set after commit. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh MemoryCreateRelationsAtomicity.tla
```

Result: 208 distinct states, 611 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase2_3 --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
