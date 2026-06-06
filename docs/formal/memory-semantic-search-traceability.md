# Memory Semantic Search Formal Verification Traceability

Status: focused high-use memory slice for `memory_semantic_search`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`memory_semantic_search` at 14 calls. The tool performs BGE-M3 vector retrieval
over active memory observations with optional scope and cognitive-tier filters.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_semantic_search` | Reject blank queries before embedding; normalize and validate tier filters; clamp result limits; reject non-1024d query vectors at the SQL boundary; return only active observations whose entity satisfies requested scope/tier filters; avoid duplicate observation rows when an entity has multiple scopes or tiers. | `tla/MemorySemanticSearchScope.tla`; `pgmcp-testing/tests/memory_phase3_2.rs`. |

## Issues Found And Corrected

The MCP tool accepted blank queries and sent them to the embedder.

Correction: blank/whitespace queries now return `invalid_params` before any
embedding call.

Tier validation used the raw optional string. Correction: tier values are
trimmed before validation and query use; blank optional tier strings are treated
as absent.

The SQL query used `LEFT JOIN memory_entity_scope` and
`LEFT JOIN memory_entity_tier` even when no filter was supplied. Because an
entity may have multiple scopes and multiple tiers, one observation could be
duplicated by join multiplicity.

Correction: scope and tier filters now use `EXISTS` subqueries. The result row
cardinality is driven by `memory_observations`, not membership table joins.

The query already rejected non-1024d embeddings and clamped SQL limits. The tool
now clamps the limit before calling the query and reports the effective limit in
the response envelope.

## Formal Model

`tla/MemorySemanticSearchScope.tla` models blank queries, invalid tiers,
non-1024d embeddings, low/high limits, scope filters, tier filters, active vs.
inactive observations, missing embeddings, and multi-scope/multi-tier entities.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankQueriesRejected` | Empty/whitespace queries produce no results. |
| `InvalidTiersRejected` | Invalid cognitive tiers produce no results. |
| `BadEmbeddingRejected` | Non-1024d query embeddings produce no results. |
| `LimitClamped` | Successful responses report the clamped limit. |
| `OutputWithinLimit` | Returned rows never exceed the effective limit. |
| `RowsMatchScopeAndTier` | Every returned observation satisfies active, embedding, scope, and tier filters. |
| `NoDuplicateObservationRows` | Returned observation ids are unique. |
| `NormalizedInputsUsed` | Query and tier filters use their normalized forms. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh MemorySemanticSearchScope.tla)
```

Result: 17 distinct states, 25 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase3_2 --build-jobs 1
```

Result: 11/11 passed. The focused suite covers direct 1024d semantic ranking,
non-1024d rejection, invalid-tier rejection, blank-query rejection, and the
multi-scope/multi-tier duplicate-row regression.
