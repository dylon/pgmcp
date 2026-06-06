# Memory RAPTOR Search Formal Verification Traceability

Status: focused high-use memory slice for `memory_raptor_search`.

## Scope

30-day durable telemetry ranked `memory_raptor_search` at 9 calls with 2
non-ok outcomes. It is the next highest-use tool in the telemetry ranking that
was not already covered by a formal ledger row.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `memory_raptor_search` | Reject blank queries before embedding; reject nonpositive `scope_id`; reject non-1024d query embeddings; normalize `levels` by rejecting empty/oversized/out-of-range lists and deduping valid levels; clamp `k` and `ef_search`; return up to `k` hits per requested level; return only requested levels; remain read-only and release the transaction-local HNSW setting. | `tla/MemoryRaptorSearchScope.tla`; `pgmcp-testing/tests/memory_phase6_7.rs`. |

## Issues Found And Corrected

The tool embedded blank queries and let malformed `levels` flow to SQL.
Correction: the tool now trims and rejects blank queries before embedding,
rejects nonpositive `scope_id`, normalizes `levels`, clamps `k` to 1..200, and
clamps `ef_search` to 1..10000.

The SQL helper validated embedding dimension but accepted empty, negative,
oversized, and duplicate level lists. Correction: `normalize_memory_raptor_levels`
is shared by the tool and direct query helper. It rejects malformed lists and
dedupes/sorts valid filters.

The query documentation promised top-k results at each requested RAPTOR level,
but SQL used a single global `LIMIT`. Dense lower levels could starve higher
summary levels. Correction: the query now uses `row_number() OVER (PARTITION BY
level ORDER BY distance, id)` and filters `level_rank <= k`, then orders the
merged per-level candidates by distance.

`SET LOCAL hnsw.ef_search = ...` was built with `format!`. The value came from
integer config, so it was not string-injection exposed, but it still accepted
nonsensical values. Correction: the helper clamps it and sets it with
`set_config('hnsw.ef_search', $1, true)`.

## Concurrency Boundary

`memory_raptor_search` is read-only. It opens a short PostgreSQL transaction
only to apply the transaction-local HNSW `ef_search` setting, performs one
SELECT, then commits. It adds no mutexes, no background work, and no shared
process state. Failed requests reject before opening the transaction.

## Formal Model

`tla/MemoryRaptorSearchScope.tla` models representative valid and invalid
queries, scope ids, embedding dimensions, level filter forms, k values, and
ef_search values.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Blank queries, bad scope ids, bad embeddings, and malformed level filters reject. |
| `SuccessfulRequestShape` | Successful requests have 1024d embeddings, nonblank query, positive scope, and bounded k/ef. |
| `LevelsNormalized` | Successful level filters are nonempty, deduped, bounded, and within range. |
| `ResultsOnlyRequestedLevels` | Returned hits come only from the requested normalized levels. |
| `PerLevelTopKBound` | Each level contributes at most k hits. |
| `NoRequestedLevelStarvation` | A requested level with available rows contributes at least one hit. |
| `ReadOnlyNoHeldLock` | The tool writes nothing and holds no lock after response. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh MemoryRaptorSearchScope.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 10 distinct states, 20
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test memory_phase6_7 --build-jobs 1
```

Result: 11/11 passed, 1 ignored model-download test skipped. The focused run
covers non-1024d rejection, malformed level rejection, per-level top-k behavior,
duplicate level normalization, MCP-level request validation, k clamping, and
dispatch coverage for adjacent memory graph tools.
