# Memory Hybrid Search Formal Verification Traceability

Status: focused memory retrieval slice for `memory_hybrid_search`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `memory_hybrid_search` in the
2-call cluster. The tool is read-only, but it sits on a mixed dense/sparse
ranking boundary, so correctness depends on rejecting invalid requests before
embedding, preserving scope/tier filters in both legs, and preventing
multi-membership rows from inflating the fused rank.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_hybrid_search` | Reject blank queries before embedding; normalize tier filters before validation/querying; clamp limits to 1..=200; reject non-1024d query embeddings in the DB layer; apply the same active/scope/tier filters in dense and sparse legs; avoid duplicate or rank-inflating rows from multi-scope/multi-tier memberships; execute read-only. | `tla/MemoryHybridSearchScope.tla`; `memory_phase3_2`. |

## Issues Found And Corrected

The MCP wrapper validated `tier` without trimming it and embedded the raw
`params.query`, so a blank query could reach the embedding backend and a valid
padded tier such as `" semantic "` was rejected. The wrapper now trims the
query, rejects blank queries before embedding, trims/normalizes tier filters,
clamps `limit` to 1..=200, passes normalized values to the query, and returns
the effective limit in the response.

The SQL dense and sparse legs used `LEFT JOIN memory_entity_scope` and
`LEFT JOIN memory_entity_tier` for filtering. With no scope or tier filter, an
entity that belonged to multiple scopes and tiers produced a cartesian product
of candidate rows. The final `GROUP BY` collapsed duplicate output rows, but
the RRF score was inflated by summing duplicate per-leg ranks. The query now
uses `EXISTS` predicates, matching `memory_semantic_search`, so each
observation contributes at most once per leg.

## Formal Model

`tla/MemoryHybridSearchScope.tla` models request normalization, embedding
dimension rejection, dense/sparse candidate filtering, and fused result output.
It includes a specific no-sparse-hit case where a worse dense hit has multiple
scope/tier memberships; the invariant requires the closest dense hit to remain
first, ruling out membership-driven RRF inflation.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankQueriesRejected` | Empty or whitespace queries reject before embedding/querying. |
| `InvalidTiersRejected` | Tier filters outside the closed vocabulary fail closed. |
| `BadEmbeddingRejected` | Non-1024d query embeddings reject in the DB boundary. |
| `LimitClamped` / `OutputWithinLimit` | Effective result limits stay in 1..=200 and bound output. |
| `RowsMatchScopeAndTier` | Returned rows satisfy the normalized active/scope/tier filter. |
| `NoDuplicateObservationRows` | One observation appears at most once in the response. |
| `NoMembershipRankInflation` | Multi-scope/tier memberships cannot make a worse dense-only hit outrank the closest dense hit. |
| `NormalizedInputsUsed` | The response/query boundary uses trimmed query and tier values. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh MemoryHybridSearchScope.tla
```

Result: 19 distinct states, 28 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase3_2 --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
