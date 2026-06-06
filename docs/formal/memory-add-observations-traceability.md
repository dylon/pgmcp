# Memory Add Observations Formal Verification Traceability

Status: focused high-use memory slice for `memory_add_observations`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`memory_add_observations` at 18 calls. The tool appends observations to existing
memory entities using the official-compatible entity-name request shape.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_add_observations` | Reject empty request batches; fail closed when an entity name resolves to multiple active entities; no-op for missing/expired entities; dedupe content per entity; insert with `agent_write` provenance. | `tla/MemoryAddObservations.tla`; `memory_phase2_3`. |

## Issue Found And Corrected

The DB append query resolved the target entity with:

```sql
SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1
```

The schema permits multiple active entities with the same name and different
`entity_type` values. The `LIMIT 1` could therefore attach observations to an
arbitrary entity.

Correction: the query now loads all active entity ids for the name. Zero matches
remain a no-op for official compatibility; more than one match returns a
protocol error that the MCP tool surfaces as `invalid_params`.

## Formal Model

`tla/MemoryAddObservations.tla` models unique, duplicate, missing, and expired
entity-name requests with existing and new observation content.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `AmbiguousNamesRejectedNoWrite` | Ambiguous active entity names reject without inserting observations. |
| `MissingOrExpiredNoWrite` | Missing or expired entities do not create observations. |
| `InsertedRowsBelongToResolvedEntity` | Inserted rows attach only to the uniquely resolved entity. |
| `InsertedRowsAreAgentWrite` | Inserted rows carry agent-write provenance. |
| `NoDuplicateEntityContent` | Active observation content is deduped per entity. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-MemoryAddObservations \
  -config docs/formal/tla/MemoryAddObservations.cfg \
  docs/formal/tla/MemoryAddObservations.tla
```

Result: 193 distinct states, 193 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase2_3 --build-jobs 1
```

Result: 9/9 passed, including the new ambiguous-active-entity-name regression.
