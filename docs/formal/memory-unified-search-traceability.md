# Memory Unified Search Formal Verification Traceability

Status: focused high-use memory retrieval slice for `memory_unified_search`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed `memory_unified_search`
at 4 calls, the next uncovered memory-graph retrieval tool after
`io_hotpath`. The tool is read-only: it embeds a normalized query, optionally
filters by unified graph node type, and queries the `memory_unified_nodes`
materialized view under a transaction-local HNSW planner setting.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_unified_search` | Reject blank/oversized queries before embedding; normalize, deduplicate, bound, and validate `node_types` against the closed ontology registry; require 1024d query embeddings before SQL; clamp `k` and `ef_search`; return only rows whose node types satisfy the normalized filter; remain read-only with no persistent locks. | `tla/MemoryUnifiedSearchBoundary.tla`; `memory_phase6_7`. |

## Issues Found And Corrected

The MCP wrapper passed raw query text to the embedder before validating it.
Blank queries could consume embedding resources and reach the DB helper.

Correction: the wrapper trims the query, rejects blank and oversized input
before embedding, and reports the normalized query in the response.

The wrapper also passed caller-supplied `node_types` directly to SQL. Empty
lists, blank entries, duplicate entries, and unknown node-type strings were
not rejected or normalized at the API boundary.

Correction: `node_types` now fail closed when malformed, are deduplicated after
trimming, and are validated against `src/db/ontology.rs::NODE_TYPES`, the same
closed vocabulary guarded by the matview golden tests.

The DB helper clamped only `LIMIT`; `SET LOCAL hnsw.ef_search` used the raw
configuration value. A negative or excessive value could fail the query or
request excessive vector-search work.

Correction: both the tool wrapper and `queries::memory_unified_search` clamp
`ef_search` to `1..=10000`. The lower-level helper also keeps the `LIMIT`
clamp for direct callers.

## Formal Model

`tla/MemoryUnifiedSearchBoundary.tla` models valid, blank, oversized, malformed
filter, bad-vector-dimension, duplicate-filter, and non-embedding-filter
requests.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Any request with a validation or vector-dimension reason is rejected. |
| `PreEmbedValidationPrecedesEmbedding` | Blank/oversized query and malformed filter requests never embed or query. |
| `BadEmbeddingDoesNotQuery` | A wrong-dimension embedding may be produced, but it never reaches SQL. |
| `SuccessfulRequestBounds` | Successful requests have 1024d embeddings and bounded `k`/`ef_search`. |
| `NodeTypeFiltersNormalized` | Supplied filters normalize to a non-empty bounded subset of registered node types. |
| `ResultsRespectFilter` | Filtered successful responses only contain requested node types. |
| `OnlyEmbeddingRowsReturned` | Vector search returns only node types with non-NULL embeddings. |
| `ReadOnlyNoHeldLock` | The model has no persistent write or held-lock path. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh MemoryUnifiedSearchBoundary.tla
```

Result: exit 0; no invariant violations; 9 distinct states and 18 states
generated at depth 1.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase6_7 unified_search --build-jobs 1
```

Result: 5/5 passed for the filtered unified-search slice.
