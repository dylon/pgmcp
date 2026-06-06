# Index Stats Formal Verification Traceability

Status: focused high-use inventory slice for `index_stats`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`index_stats` at 13 calls. The tool reports live in-memory counters, DB-backed
index counts, and optional workspace-wide effect counts.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `index_stats` | Increment the MCP request counter before snapshotting; always return a parseable JSON envelope with `snapshot`, `index`, and `effect_breakdown`; report DB counts exactly when available; degrade only the `index` block if count queries fail; keep effect enrichment optional and non-fatal. | `tla/IndexStatsEnvelope.tla`; `pgmcp-testing/tests/mcp_tool_smoke.rs`. |

## Issues Found And Corrected

`index_stats` returned the in-memory `StatsTracker` snapshot but did not include
the DB-backed index counters (`projects`, `indexed_files`, `chunks`, and
`total_bytes`) exposed by `DbClient`. Existing smoke tests only searched for
loose substrings, so that omission was not pinned precisely.

Correction: the tool now includes an `index` object. When DB count queries
succeed, it reports exact nonnegative counts with `available: true`. If a count
query fails, the response remains JSON and the `index` block reports
`available: false` with zero counts and an error string; the live `snapshot`
and optional effect enrichment remain available.

## Formal Model

`tla/IndexStatsEnvelope.tla` models successful and failed DB count reads, raw
pool present/absent for effect enrichment, effect query failure, and concurrent
MCP request increments between the local `fetch_add` and the snapshot reads.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `SnapshotAlwaysPresent` | DB/enrichment failures never remove the live stats snapshot. |
| `LocalRequestIncrementVisible` | The tool's own request increment is reflected in the snapshot. |
| `IndexCountsExactWhenAvailable` | Successful DB count reads appear exactly in the `index` block. |
| `DbFailureOnlyDisablesIndexBlock` | DB count failure is localized to `index.available=false`. |
| `AllCountsNonNegative` | Snapshot, DB, and effect counts stay in the natural-number domain. |
| `EffectBreakdownGraceful` / `EffectBreakdownExactWhenAvailable` | Effect enrichment is optional and exact when the raw-pool query succeeds. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh IndexStatsEnvelope.tla)
```

Result: 4 distinct states, 8 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke index_stats --build-jobs 1
```

Result: 4/4 passed. The filtered smoke run covers nonzero mock counts, zero
mock counts, nonzero byte totals, and parseable JSON output.
