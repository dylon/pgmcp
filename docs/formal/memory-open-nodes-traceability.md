# Memory Open Nodes Formal Verification Traceability

Status: focused memory read slice for `memory_open_nodes`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `memory_open_nodes` at 3
calls. The tool is read-only: it accepts exact entity names, opens active
entities, returns their active observations, and reports active relations.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_open_nodes` | Reject empty, blank, and oversized name lists; trim and dedupe exact names; bound the read fan-out; open active entities only; return active observations only; suppress relations whose other endpoint is inactive; remain read-only with no persistent locks. | `tla/MemoryOpenNodesScope.tla`; `oracle_memory_open_nodes`. |

## Issues Found And Corrected

The tool accepted unbounded name lists and passed names through without trimming
or deduplication.

Correction: names now normalize before SQL, blank names reject, and the request
is capped at 100 names.

Relation reads filtered `memory_relations.valid_to IS NULL`, but did not require
both joined endpoint entities to be active. Since entity deletion is bi-temporal
and does not rewrite every relation row, opening an active entity could surface a
relation to a soft-deleted entity.

Correction: incoming and outgoing relation queries now require both endpoint
`memory_entities` rows to have `valid_to IS NULL`.

Opened entity and relation ordering was not deterministic.

Correction: entity and relation rows now have stable `ORDER BY` clauses.

## Formal Model

`tla/MemoryOpenNodesScope.tla` models name-list validation, trim/dedupe
normalization, bounded fan-out, active entity and observation filtering,
inactive relation endpoints, deterministic active relation output, and
read-only execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidNamesReject` | Empty, blank, and oversized name lists reject without opened nodes. |
| `NameListBounded` | Accepted requests stay under the configured name cap. |
| `NamesNormalizedAndDeduped` | Trimmed duplicate names collapse to one exact lookup. |
| `ActiveEntitiesOnly` | Opened nodes are active entities only. |
| `ActiveObservationsOnly` | Soft-deleted observations are not surfaced. |
| `ActiveRelationEndpointsOnly` | Relations to inactive endpoints are not surfaced. |
| `ReadOnlyNoHeldLock` | The model has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_memory_open_nodes --build-jobs 1
```

Result: 2/2 passed for the focused memory-open-nodes oracle suite.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh MemoryOpenNodesScope.tla
```

Result: TLC exit 0; 4 distinct states, 8 states generated; no invariant
violations.
