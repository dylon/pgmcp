# Memory Search Nodes Formal Verification Traceability

Status: focused high-use memory slice for `memory_search_nodes`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`memory_search_nodes` at 25 calls. The tool is the official-compatible
substring search across active memory entities and observations.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_search_nodes` | Reject empty queries; treat SQL LIKE metacharacters as literal substring text; scope-filter by membership without multiplying observation rows; count active matching observations exactly once; clamp result limit to `1..=500`. | `tla/MemorySearchNodesScope.tla`; `pgmcp-testing/tests/memory_eval.rs`; `src/db/queries/memory_search.rs` unit test. |

## Issues Found And Corrected

The query built `ILIKE '%{query}%'` directly. Because `%`, `_`, and backslash
were not escaped, a query like `%` acted as an unbounded wildcard instead of a
literal substring search.

Correction: `memory_search_nodes` now builds an escaped pattern and adds
`ESCAPE '\\'` to every `ILIKE` predicate.

The SQL also left-joined `memory_entity_scope` directly. For unscoped searches,
an entity attached to multiple scopes multiplied each active observation before
aggregation, inflating `matched_observations` and potentially changing ranking.

Correction: scope filtering now uses an `EXISTS` predicate. Scope membership can
include or exclude an entity, but it cannot duplicate observation rows.

## Formal Model

`tla/MemorySearchNodesScope.tla` models active/inactive entities, active/inactive
observations, multi-scope membership, literal wildcard queries, scoped queries,
and limit clamping.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `RowsAreActiveAndScoped` | Returned rows are active, matched, and allowed by the requested scope. |
| `MatchedObservationCountExact` | `matched_observations` equals the number of active matching observations, independent of scope count. |
| `WildcardQueryIsLiteral` | The modeled `%` query does not match every row as a wildcard. |
| `OutputWithinLimit` | Returned rows never exceed the effective limit. |
| `EffectiveLimitClamped` | The effective limit is the clamped caller limit. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-MemorySearchNodesScope \
  -config docs/formal/tla/MemorySearchNodesScope.cfg \
  docs/formal/tla/MemorySearchNodesScope.tla
```

Result: 928 distinct states, 928 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test memory_eval --build-jobs 1
```

Result: 26/26 passed. The new regressions cover literal `%` search and
multi-scope observation-count stability.

```bash
cargo test -p pgmcp ilike_substring_pattern_escapes_sql_wildcards --lib
```

Result: 1/1 passed for the escaped-pattern helper.
