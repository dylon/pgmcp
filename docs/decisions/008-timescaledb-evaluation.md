# ADR-008: Do not adopt TimescaleDB; stay on plain PostgreSQL for time-series tables

**Status:** Accepted
**Date:** 2026-05-27

## Context

The unified knowledge-graph work (the temporal graph-RAG stage) raised the
question of whether **TimescaleDB** would benefit pgmcp's genuinely
time-series-shaped tables: `mcp_tool_calls` (tool telemetry), `experiment_results`
/ measurements, and the append-only logs (`work_item_progress`,
`work_item_status_history`, `work_item_claims`, `agent_presence`,
`agent_outcomes`, `session_prompts`, `git_commits`).

TimescaleDB offers hypertables (time-partitioned chunks), continuous aggregates
(incrementally-refreshed time-bucket rollups), columnar compression, retention
policies, and `timescaledb_toolkit` hyperfunctions (time-weighted averages,
approx percentiles, counter aggregates).

This is the time-series analogue of **ADR-001** (which declined *pgvectorscale*,
Timescale's vector index, for the vector path).

## Method (data-driven)

Per the benchmarking mandate, the decision is grounded in measured scale rather
than assumption. Environment probe (2026-05-27, local install):

| Probe | Result |
|---|---|
| `timescaledb` in `pg_available_extensions` | **2.27.1, available, NOT installed** |
| `vector` (pgvector) | 0.8.2, installed |
| `mcp_tool_calls` rows | **3,613** |
| `session_prompts` rows | 1,239 |
| `git_commits` rows | 258 |
| `experiment_results` rows | 3 |

The largest time-series table holds **~3.6 K rows**. TimescaleDB's hypertable /
continuous-aggregate / compression machinery is engineered for and pays off at
**10⁷–10⁹ rows**; the planning literature and Timescale's own guidance put the
crossover where a single table no longer fits comfortably in memory or where
time-range scans dominate a multi-GB heap. pgmcp's time-series data is **3–6
orders of magnitude below** that crossover.

A formal latency/throughput benchmark (plain btree-on-time + partial indexes vs
hypertables + continuous aggregates) was therefore **not run**: at thousands of
rows every candidate query is an index scan or a full-heap scan of a few hundred
KB — sub-millisecond on plain PostgreSQL — so the comparison is a foregone
conclusion (any hypertable chunk-exclusion benefit is unmeasurable against a
table that fits in a single page-cache run, while the extension adds fixed
overhead). The benchmark harness below is recorded so it can be run verbatim if
the *reconsider triggers* fire.

## Decision

**Do not adopt TimescaleDB.** Keep the time-series tables on plain PostgreSQL
with btree-on-timestamp indexes (already present, e.g. `mcp_tool_calls(ts)`,
`git_commits(author_date)`), adding partial / composite indexes as query
patterns warrant.

## Rationale

1. **Scale mismatch (decisive).** ~3.6 K max rows vs TimescaleDB's 10⁷–10⁹
   sweet spot. Plain btree range scans are already sub-millisecond; there is no
   latency to reclaim and no storage pressure to compress.
2. **The graph-relevant temporal feature is *not* what TimescaleDB
   accelerates.** The temporal graph-RAG work relies on **bitemporal
   interval** queries (`memory_facts_at`, the Stage-5a edge `valid_from`/
   `valid_to` filtering): "rows valid at instant T" is an *interval-overlap*
   predicate. Hypertables partition on a **single** time axis (chunk exclusion
   on one column); they do not accelerate interval-overlap. The right tool is a
   btree/GiST range index (Stage 5a adds `idx_memory_unified_edges_valid` on
   `(valid_from, valid_to)`) — which needs no extension.
3. **Deployment friction (same logic as ADR-001).** TimescaleDB is another
   extension to install, version-couple to the PostgreSQL major, and carry
   across every clone. pgmcp is a local, single-developer systemd service with
   no hosting bill to optimize; the operational cost outweighs a benefit that
   is currently zero.
4. **No cost pressure.** TimescaleDB's headline value (compression, cheaper
   retention) targets cloud storage bills. There is none here.
5. **Coexistence is fine but moot.** TimescaleDB and pgvector compose without
   conflict, so adoption would not break the vector path — but with no benefit
   to gain, compatibility does not change the decision.

## Out of scope

- **Vector search** — settled by ADR-001 (stay on pgvector HNSW).
- **Bitemporal memory interval queries** — served by range indexes, not
  hypertables (see Rationale #2).

## When to reconsider

Re-run the benchmark below and revisit this ADR if **any** of:

- `mcp_tool_calls` (or any single time-series table) exceeds **~10 M rows**, or
  its on-disk size exceeds available page cache and time-range scans become a
  measured bottleneck.
- pgmcp pivots to a **shared/hosted, multi-tenant** deployment where telemetry
  from many users accumulates and storage cost / retention becomes real.
- The scientific-experiment subsystem grows to where **`timescaledb_toolkit`
  hyperfunctions** (time-weighted averages, approx percentiles over large
  measurement series) would replace materially more hand-rolled SQL than they
  cost to adopt.

## Benchmark harness (to run if a trigger fires)

```bash
# Plain PostgreSQL baseline (current):
psql -c "EXPLAIN (ANALYZE, BUFFERS)
         SELECT date_trunc('hour', ts) h, count(*) FROM mcp_tool_calls
         WHERE ts > now() - interval '30 days' GROUP BY 1 ORDER BY 1;"

# TimescaleDB candidate (in a scratch DB):
#   CREATE EXTENSION timescaledb;
#   SELECT create_hypertable('mcp_tool_calls','ts', migrate_data => true);
#   CREATE MATERIALIZED VIEW mcp_calls_hourly WITH (timescaledb.continuous) AS
#     SELECT time_bucket('1 hour', ts) h, count(*) FROM mcp_tool_calls GROUP BY 1;
# Then compare p50/p99 latency (hyperfine), ingest throughput, and on-disk size
# with CPU affinity + max frequency, tee'd to a file (per the benchmarking rules).
```

Decision recorded after the unified-graph temporal work (Stages 5a–5e) landed
the `valid_from`/`valid_to` interval columns + range index, which is the
graph-side temporal capability TimescaleDB would *not* have provided.
