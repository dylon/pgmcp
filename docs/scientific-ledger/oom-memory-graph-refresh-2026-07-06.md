# Scientific Ledger — `memory-graph-refresh` OOM balloon (2026-07-06)

**Status:** implemented; `cargo check` green; unit tests added. Pending the final
`./scripts/verify.sh` gate and post-deploy live verification (below).

**Precedent:** `docs/scientific-ledger/oom-fix-2026-04-22.md` (an earlier OOM fix).

---

## 1. Incident

The `pgmcp.service` systemd **user** unit was kernel-OOM-killed roughly once a day.
`journalctl --user -u pgmcp` over 5 days:

| kill (EDT)        | cgroup memory peak | lifetime CPU |
|-------------------|--------------------|--------------|
| Jul 02 04:41      | 97.2 GB            | 7h51m        |
| Jul 02 13:01      | 96.7 GB            | 7h23m        |
| Jul 02 19:02      | 82.9 GB            | 7h22m        |
| Jul 04 11:18      | 94.5 GB            | 2d07h        |
| Jul 04 19:44      | 86.3 GB            | 8h32m        |
| Jul 04 21:31      | 43.5 GB            | 1h03m        |
| Jul 05 17:19      | 95.7 GB            | 16h55m       |
| Jul 06 09:55      | 80.5 GB            | 8h32m        |

Each: `Main process exited, code=killed, status=9/KILL` → `Failed with result
'oom-kill'`. Healthy restarts peak at **~9.2–9.4 GB** — a ~10× balloon. Kill level
varies (43–97 GB) with concurrent load — the signature of unbounded growth, not one
fixed oversized allocation. Host: 125 GiB RAM + 542 GiB swap **at 0 B used**
(`vm.swappiness=0`); no cgroup `MemoryMax`.

## 2. Root-cause analysis

`cron_run_history` records per-run `rss_mb_start/end/delta`. Sorted by single-run
delta, one job dominates:

```
job                    started       dur   start_mb → end_mb    Δ_mb
memory-graph-refresh   06-26 03:23   631s   2371 →  42026     +39,655
memory-graph-refresh   07-02 10:43   631s   2012 →  25266     +23,254
memory-graph-refresh   07-04 01:04   617s  54641 →  66269     +11,628   (starts at 54 GB)
memory-graph-refresh   07-03 01:04   625s  66751 →   3513     −63,238   (reclaimed later)
```

Every other cron's max delta is < 1 GB. High-`end_mb` light crons (stats-aggregation,
mcp-client-liveness) show ~0 delta — they merely *sample* the already-ballooned
process.

### Two-part mechanism (both verified)

```
 ┌─────────────────────────── TRIGGER (every 6 h) ───────────────────────────┐
 │ memory-graph-refresh (was an UNGATED *light* cron: no serialization, no    │
 │ memory gate) → REFRESH MATERIALIZED VIEW CONCURRENTLY of                   │
 │   memory_unified_nodes  = 1.03M rows / 9.2 GB                              │
 │     = 232 MB heap + 4.0 GB TOAST (inline embedding vector(1024))           │
 │       + 4.9 GB HNSW index on that embedding                               │
 │   → rebuilds the whole matview + reindexes the 4.9 GB HNSW over 1M         │
 │     vectors EVERY time, even when nothing changed. PG tuned small          │
 │     (maintenance_work_mem=64 MB) → heavy spill → ~10 min of DB saturation. │
 └───────────────────────────────────┬───────────────────────────────────────┘
                                     │  (DB is a SEPARATE service — its memory
                                     │   is NOT in pgmcp's cgroup)
                                     ▼
 ┌───────────────────────── AMPLIFIER (pgmcp's own heap) ─────────────────────┐
 │ During the 10-min DB stall, pgmcp's concurrent request/result/ingest       │
 │ buffers pile up (can't drain to the stalled DB). With MALLOC_ARENA_MAX=2   │
 │ glibc drives both arenas' high-water marks to tens of GB and NEVER         │
 │ madvise()s them back → retention, not a logical leak:                      │
 │   • reclaimable  → the −63 GB deltas (a later trim/reuse returns it)       │
 │   • stacks across runs → runs "start" at 54–68 GB                         │
 │   • swappiness=0 → that anon RSS is never swapped → OOM despite 542 GB swap │
 └───────────────────────────────────────────────────────────────────────────┘
```

**Ruled out (verified bounded / unreachable):** the DB-outage outbox (disk-backed,
capped — `src/health/outbox.rs`); the reactive ingest channel (bounded `try_send`);
`bulk_extract_embeddings(None)` (mmap-gated for >50k chunks; 769k corpus never hits
the in-RAM path); a baseline leak (idle RSS flat ~1.5 GB over 10 min); PG memory
(kill is pgmcp's PID; RSS from `/proc/self/statm`).

## 3. Fix (phased)

| Phase | Change | Files |
|-------|--------|-------|
| **P0.1** | `malloc_trim(0)` after every heavy-cron body + on the memory-watchdog pressure edge — converts arena retention from cumulative-and-fatal to transient-and-reclaimed. Gated by `[mem_guard] trim_after_heavy_cron` (default on) via a process-wide tunable set at startup (the `a2a::client::set_default_timeout_secs` idiom). | `src/stats/rss.rs` (`trim_malloc`), `src/cron/scheduler.rs` |
| **P0.2** | Route `memory-graph-refresh` through the **gated heavy path** (`register_heavy_cron`): serialized on `heavy_cron_lock`, passes `try_heavy_gate` (incl. the new memory gate), inherits the cooldown one-shot retry. Removed the old ungated `spawn_recorded`. | `src/cron/scheduler.rs`, `src/cli/daemon.rs` |
| **P0.3** | `SET LOCAL maintenance_work_mem` on the refresh session — slashes spill / DB-saturation. Configurable via `[cron] matview_refresh_maintenance_work_mem_mib` (default 1024; `0` = leave the cluster default) via a process-wide tunable set at startup, so the query layer needn't thread config through the refresh callers. | `src/db/queries/memory_search.rs` |
| **P1** | `MemoryPressure` flag + `spawn_memory_watchdog` (pure `mem_decide`, two-axis hysteresis: low-avail + high-RSS) → gate in `try_heavy_gate`, embed intake, and a **defer-until-RAM-recovers** retry (bounded, `MEM_PRESSURE_MAX_RETRIES`). New `SkipReason::MemoryPressure` (+ v69 CHECK migration). `[mem_guard]` config. | `src/health/memory_pressure.rs` (new), `src/health/watchdog.rs`, `src/stats/tracker.rs`, `src/stats/tracker/outcomes.rs`, `src/cron/history/vocab.rs`, `src/db/migrations/v69_memory_pressure_skip_reason.rs` (new), `src/cron/scheduler.rs`, `src/embed/pool.rs`, `src/config.rs`, `src/cli/daemon.rs` |
| **P2.2** | Data-change gate: skip the refresh when the `file_chunks` fingerprint is unchanged AND the last refresh is within `memory_graph_refresh_max_staleness_secs` (24 h backstop). | `src/cron/memory_graph_refresh.rs`, `src/config.rs` |
| **P2.3** | Lean `find_project_id_by_cwd` (no correlated `COUNT(*)`) for the two high-frequency loop callers (ingest ~200 ms, liveness 30 s) — removes the ~1.2 s query storm that co-occurs with a refresh — **plus** a process-wide TTL cwd→id cache (`[clients] cwd_project_cache_ttl_secs`, default 30; size-capped) so the lookup avoids the DB entirely, including while a refresh has the DB saturated. | `src/db/queries/projects.rs`, `src/proc_clients/ingest.rs`, `src/cron/mcp_client_liveness.rs` |
| **P3** | Slim the 9.2 GB matview: split into lean `memory_unified_nodes` (node_id/type/label/importance — cheap refresh for the traversal tools) + `memory_unified_node_vectors` (node_id/type/embedding + HNSW — the expensive one, built LAST for crash-safety). `memory_unified_search` joins them, with a `to_regclass` fallback to the legacy inline-embedding query during the post-deploy transition. Self-heal extended (`ensure_vectors_only`). Hash marker `split:nodes+vectors:v1` forces the one-time rebuild. Every caller that adds *searchable* embedded nodes (`memory-graph-refresh`, `memory-concepts`) refreshes the vectors matview too (`42P01`-tolerant during the transition); traversal-only callers (`ontology_*`) correctly stay nodes+edges — no search-freshness regression. | `src/db/migrations.rs`, `src/db/queries/memory_search.rs`, `src/db/ontology.rs` (tests preserved) |
| **P2.1** | systemd `MemoryHigh=48G` (soft) + `MemoryMax=96G` (hard) backstops. | `~/.config/systemd/user/pgmcp.service` |

**Why P0.1 is the keystone:** the amplifier is glibc arena retention, so `malloc_trim`
alone converts every balloon from fatal-and-cumulative to transient-and-reclaimed.
P0.2/P0.3/P2.2 shrink and gate the trigger; P1 waits for RAM before running heavy
work; P3 removes the every-refresh HNSW rebuild at its source; P2.1 is the last resort.

## 4. Swap diagnosis (why 542 GiB swap never engaged)

- **`vm.swappiness = 0`** — the kernel will not proactively swap anon heap; it drops
  page cache then OOMs. Decisive.
- **Priority inverted for RAM protection:** `/dev/zram0` (31 GB zstd, **RAM-backed**)
  is priority **100** (tried first, can't relieve RAM exhaustion, f32 vectors
  compress poorly); the real 511 GB NVMe `/dev/nvme0n1p3` is priority **−1**.
- cgroup swap uncapped (`memory.swap.max = max`).

**Operator recommendations (outside the binary; highest-impact first):**
1. Raise `vm.swappiness` to ~10–60 so a reclaimable balloon spills to the NVMe swap.
2. Lower `/dev/zram0` priority below the NVMe partition (or shrink zram).
3. `MemoryHigh` (P2.1) + swappiness > 0 = the cgroup spills to NVMe before the OOM.
   `malloc_trim` (P0.1) makes all of this rarely matter.

## 5. Verification

**In-session (done):** `cargo check --bin pgmcp` green; pure unit tests for
`mem_decide` (two-axis hysteresis) and the P2.2 `is_unchanged` gate.

**Final gate:** `./scripts/verify.sh` with `CARGO_BUILD_JOBS=6` (Gate 5 OOMs the
memory scope at 32-way).

**Post-deploy (after `systemctl --user daemon-reload && systemctl --user restart pgmcp`):**
- **OOM gone:** over 24–48 h, no `oom-kill` in `journalctl --user -u pgmcp` (kills
  were ~daily).
- **Balloon gone:** `memory-graph-refresh` `rss_mb_delta` in `cron_run_history`
  collapses from +20–40 GB / −63 GB swings to ~0; `NoOp` rows when the corpus is
  unchanged; `pgmcp_peak_rss_bytes` (`:9464/metrics`) tracks the ~1.5–9 GB baseline.
- **P3 recall (safety condition):** before/after the split, compare
  `memory_unified_search` (via the `memory_*` MCP tools) recall + latency on a fixed
  query set. **If recall regresses, revert P3** (see Rollback) — P0–P2 already make
  the daemon OOM-safe.
- **Split applied:** after restart, `\d memory_unified_nodes` shows NO embedding
  column; `memory_unified_node_vectors` exists with the HNSW; the boot log shows the
  hash-gated rebuild ("definition changed … rebuilding").

## 6. Rollback

- **P3 only** (if recall regresses): revert the `src/db/migrations.rs` +
  `src/db/queries/memory_search.rs` P3 hunks (restore the inline-embedding
  `MEMORY_UNIFIED_NODES_SQL` + single-matview build + original `memory_unified_search`).
  The next restart's hash-gate rebuilds the original single matview. P0–P2 stay.
- **All:** `git revert` the commit; the v69 CHECK widening is forward-compatible
  (a superset), so it need not be reverted.

## 7. Post-deploy findings (2026-07-07, Boy-Scout)

Deployed the release binary (`install → ~/.local/bin/pgmcp`, `daemon-reload`,
`restart`). **Result: the OOM balloon is gone** — the new daemon idles at **1.57 GB
RSS** (vs the 49–97 GB pre-fix peaks in `journalctl --user -u pgmcp`), and the
systemd caps are live (`MemoryHigh=48G`, `MemoryMax=96G`). Two latent bugs, unrelated
to the OOM but surfaced by the post-deploy log + the restart-triggered matview
rebuild, were fixed under the Boy-Scout rule:

1. **`pgmcp-embed-monitor` duration-underflow panic** (`src/embed/pool.rs`,
   `sleep_with_shutdown`). `Duration - Duration` panics on underflow; the
   `while start.elapsed() < dur` check can be crossed by a preempted/loaded thread
   before the `dur - start.elapsed()` on the next line runs (observed 07:38 under
   load: "overflow when subtracting durations"). Fixed with `saturating_sub`
   (→ `ZERO` ⇒ the loop exits cleanly).

2. **Unified-matview rebuild canceled by the 30 s `statement_timeout`**
   (`src/db/migrations.rs`). The restart's hash-gated rebuild (`build_memory_unified_views`)
   was canceled — the heavy `memory_unified_edges` (EXISTS gates over `file_symbols`)
   and `memory_unified_node_vectors` (materializing ~1M embedding vectors)
   `CREATE MATERIALIZED VIEW`s exceed the daemon's 30 s default `statement_timeout`
   (`src/db/pool.rs`) on the grown corpus, but only the HNSW *index* build had the
   extended timeout (`build_hnsw_index`). The edges build timed out first, aborting
   before the vectors matview was created — leaving `memory_unified_nodes` (lean) +
   `memory_unified_edges` present but `memory_unified_node_vectors` **missing**, so
   `memory_unified_search` lost its HNSW. **Fix:** a new `execute_matview_build`
   helper runs each heavy materialization (nodes / edges / vectors) in a transaction
   with the same extended `statement_timeout` + `maintenance_work_mem` as
   `build_hnsw_index`; `config` is threaded through `create_memory_unified_edges` /
   `ensure_edges_only`. Validation: re-verify (`scripts/verify.sh`) + re-deploy, then
   confirm live that the vectors matview + its HNSW build to completion and
   `memory_unified_search` is restored (this fix's logic is not exercised by the
   dormant real-DB test suite, so the live re-deploy is its verification).

## 8. Refresh-cadence split (2026-07-07, operator request)

The single 6-hourly `memory-graph-refresh` (all three matviews together) was split
into two independently-scheduled heavy crons so the CHEAP structural graph can stay
fresh at a short interval without paying the expensive HNSW re-maintenance on every
tick — the P3 nodes/vectors split makes this clean:

- **`memory-graph-refresh`** now refreshes only `memory_unified_nodes` +
  `memory_unified_edges` (the graph the traversal tools walk), on
  `[cron] memory_graph_refresh_interval_secs` (set to **300 = 5 min**).
- **`memory-vectors-refresh`** (new) refreshes `memory_unified_node_vectors` + its
  HNSW (the semantic-search index `memory_unified_search` seeds from), on
  `[cron] memory_vectors_refresh_interval_secs` (set to **1800 = 30 min**).

Each has its own data-change gate + `pgmcp_metadata` watermark
(`memory_graph_refresh_watermark` / `memory_vectors_refresh_watermark`), shares the
24 h `max_staleness` backstop, and runs on the same gated heavy path (serialized on
`heavy_cron_lock`, memory-pressure defer-and-retry, post-body `malloc_trim`) — so
the 5-min structural cadence is safe. Net effect: a newly-indexed file appears in
graph traversal within ~5 min; its embedding becomes semantically searchable within
~30 min. Defaults stay 6 h / 6 h (no behavior change for other installs).
Files: `src/cron/memory_graph_refresh.rs`, `src/cron/scheduler.rs`, `src/config.rs`.

## 9. Second balloon — the light-cron trim gap (2026-07-08, the real root cause)

**Incident.** ~10 h after the v4 deploy the daemon reached **50.9 GB RSS (all
anonymous heap) and became unresponsive** (throttled by `MemoryHigh=48G` into a
DB-probe-timeout thrash spiral; not OOM-killed — the cgroup cap held). Restarting
recovered it to ~1.05 GB.

**Root cause — my P0.1 trim was scoped too narrowly.** `malloc_trim(0)` ran ONLY
inside `register_heavy_cron` (scheduler.rs), covering the 18 heavy crons. The
`cron_run_history` RSS deltas indict the crons that are NOT heavy — scheduled via
`schedule_recurring`/`spawn_recorded`, so never trimmed:

```
job                  kind               max_rss_delta
project-deps-index   schedule_recurring   +2724 MB      ← untrimmed, retained
target-cleanup       schedule_recurring   +2070 MB      ← untrimmed, retained
graph-analysis       register_heavy_cron  +319 MB       (trimmed → bounded)
mcp-client-liveness  schedule_recurring   +226 MB       ← untrimmed
```

Each non-heavy cron's large transient allocation inflates the (`ARENA_MAX=2`)
glibc arena high-water and is never handed back, so it **accumulates across runs**
into a tens-of-GB balloon over hours — the *same* arena-retention mechanism as §2,
just a source the heavy-only trim missed. (The watchdog also trims only on the
pressure *edge* and pauses only *heavy* crons + intake, so it neither reclaimed
continuously nor stopped the light crons.) The original diagnosis fixated on the
single biggest single-run delta (`memory-graph-refresh`, +39 GB) and missed that
the *aggregate* of smaller, more frequent, untrimmed crons is equally fatal.

**Fix — reclaim generally, not per-heavy-cron.** A **poll-cadence `malloc_trim(0)`
in the memory-watchdog loop** (`src/health/watchdog.rs`), fired every
`poll_interval_secs` whenever `RSS ≥ [mem_guard] trim_above_rss_mib` (default 4096
MiB, `0` disables). This is source-agnostic — it reclaims retained arena high-water
from ANY origin (heavy crons, the non-heavy crons above, the request path), so the
balloon can never build regardless of which code path allocates. It keeps RSS
bounded near the 4 GiB floor, far below the 40 GiB pause / 48 GiB `MemoryHigh` /
OOM ceilings. The per-heavy-cron trim (§P0.1) stays as an immediate belt-and-braces.

**Validation (done, 2026-07-08).**

1. **Mechanism confirmed by controlled experiment** (`scratchpad/trimfrag.c`, built
   `MALLOC_ARENA_MAX=2` to match the daemon): allocate 4 GiB in 64 KB arena chunks,
   then free 63 of every 64 (fragmenting — 1/64 stays live). RSS stays **4095 MB**:
   glibc does NOT auto-return the freed pages because live chunks trap them — the
   *exact* balloon mechanism (contrast: freeing ALL chunks auto-reclaims to 2 MB
   with no trim, which is why the balloon needs scattered survivors). Then
   **`malloc_trim(0)` reclaims 4095 → 70 MB (98 %)**. So the retention is
   fragmentation and the trim genuinely returns it — the assumption the P0.1 fix
   rested on, now tested rather than assumed.

2. **Live, 40 min post-deploy** (`pid` fresh at 1.05 GB): RSS held **1.39–1.68 GB**
   (peak 1.68) while the culprit crons fired — `mcp-client-liveness` ×101,
   `target-cleanup` ×2, `project-deps-index` ×1 — vs their +226 MB/+2 GB/+2.7 GB
   per-run deltas in the 10 h run; health 200 throughout. (RSS didn't reach the
   4 GiB floor in that window, so the periodic trim had nothing to reclaim yet — the
   experiment in (1) is what proves it reclaims once RSS does climb.)

**What also held over the 10 h before recovery** (so §3–§8 are sound): no
`oom-kill`; the split crons ran on cadence (`memory-graph-refresh` 129× / 5 min,
`memory-vectors-refresh` 22× / 30 min); the vectors matview built (811,834 rows);
the refresh crons showed **0 MB** RSS delta (they are NOT a balloon source).

**Separate follow-up (flagged, not yet fixed):** `stale-cleanup`'s
`cleanup_stale_files()` occasionally hits the daemon's 30 s `statement_timeout` on
the grown 769k-chunk corpus (one historical failure) — a query-timeout issue, not a
memory one; the same class as §7's matview-build fix, to be given an extended
timeout in a focused pass across the direct-query crons.
