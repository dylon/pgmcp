# Experiment Search Formal Verification Traceability

Status: focused experiment retrieval slice for `experiment_search`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `experiment_search` at 3
calls. The tool is read-only: it validates a natural-language query, normalizes
optional filters, prefers vector search when query embedding succeeds, falls
back to full-text search when embedding fails, and returns matching experiment
summaries.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_search` | Reject blank queries and invalid filters; normalize kind/verdict filters; bound result limits; preserve project/kind/verdict filters in vector and FTS modes; use only active hypotheses for verdict filtering; expose the effective search envelope; remain read-only with no persistent locks. | `tla/ExperimentSearchScope.tla`; `oracle_experiment_search`. |

## Issues Found And Corrected

The FTS fallback path ignored `verdict`, while vector search applied it.

Correction: `experiment_search_fts` now receives and applies the same verdict
filter as vector search.

The vector verdict filter considered any historical hypothesis row, not just
the active one. A superseded accepted hypothesis could satisfy an `accepted`
filter even when the current verdict was rejected.

Correction: both vector and FTS paths now require `experiment_hypotheses.valid_to
IS NULL` for verdict filtering.

Kind and verdict filters were passed through raw. Unknown values returned empty
results instead of making the request contract explicit.

Correction: filters are trimmed, lowercased, and validated against the enum
vocabulary before querying.

The response did not expose the normalized limit or search mode, making fallback
contract regressions hard to observe.

Correction: the response includes `query`, `project_id`, `kind`, `verdict`,
`limit`, and `search_mode`.

## Formal Model

`tla/ExperimentSearchScope.tla` models blank/invalid requests, closed filter
normalization, bounded limits, vector-vs-FTS mode selection, active-verdict
matching, stale historical verdict rows, project/kind scoping, and read-only
execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Blank queries and invalid kind/verdict filters reject without results. |
| `FiltersClosed` | Accepted filters are drawn from the experiment enum vocabularies. |
| `LimitBounded` | Accepted responses expose a `1..=100` result limit. |
| `ModeMatchesEmbeddingOutcome` | Embedding success uses vector mode; embedding failure uses FTS mode. |
| `ProjectFilterSound` | Project-filtered responses contain only that project. |
| `KindFilterSound` | Kind-filtered responses contain only that kind. |
| `ActiveVerdictFilterSound` | Verdict-filtered responses match active hypothesis verdicts. |
| `StaleVerdictsIgnored` | Historical superseded verdicts cannot satisfy the filter. |
| `FallbackFilterParity` | FTS fallback preserves the vector path's project/kind/verdict filters. |
| `ReadOnlyNoHeldLock` | The model has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_experiment_search --build-jobs 1
```

Result: 2/2 passed for the focused experiment-search oracle suite.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh ExperimentSearchScope.tla
```

Result: TLC exit 0; 6 distinct states, 12 states generated; no invariant
violations.
