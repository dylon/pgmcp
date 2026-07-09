# OOM root cause CORRECTED: `fuzzy-sync`, not `memory-graph-refresh`

**Supersedes the diagnosis in** `oom-memory-graph-refresh-2026-07-06.md`.
**Date:** 2026-07-08. **Fix commit:** `f5b13de` (main).

The 2026-07-06 ledger attributed the recurring OOM to `memory-graph-refresh` +
glibc arena retention, and shipped P0–P3 (heavy-cron gating, a memory-pressure
watchdog with `malloc_trim`, a matview split, systemd caps). Those are sound
hardening and stay. **But they did not stop the OOM, because the diagnosis was
wrong on two counts.** This record corrects it via the scientific ledger the
CLAUDE.md mandates: each hypothesis, its test, and the result.

## H1 (prior): `memory-graph-refresh` + glibc arena retention → REFUTED as root cause

- The prior ledger's `malloc_trim` fix was a **no-op**: pgmcp installs **mimalloc**
  as its `#[global_allocator]` (`src/main.rs:16`). `libc::malloc_trim` operates on
  the glibc heap, which the daemon does not use. The "98% reclaim" micro-benchmark
  linked glibc, so it never described the daemon.
- **Test:** deployed the trim fixes; the daemon re-ballooned to 48 GB. **Refuted.**
- `memory-graph-refresh` has real multi-GB refresh bursts, but they are not the
  dominant, recurring balloon.

## H2: a specific ~hourly cron is the root cause (user's decisive hint)

- **Test:** correlate `cron_run_history.rss_mb_delta` with cadence over 4 days.
- **Result:** `fuzzy-sync` — avg **+773 MB** on **46 of 48 runs**, ~every 87 min —
  the most consistent leaker (`target-cleanup` +1046 MB/34 min was the only rival).
  My earlier flat 24-min RSS watches never spanned a `fuzzy-sync` run. **Confirmed
  direction.**

## H3: `fuzzy-sync` builds oversized tries entirely in RAM → CONFIRMED

- `fuzzy-sync` (`src/cron/fuzzy_sync.rs::run_fuzzy_sync`) rebuilds ~299 disk-backed
  fuzzy tries (symbols/paths/commits × 99 projects + global mandates/concepts) from
  PostgreSQL each run. The old `rebuild_*` did whole-table `fetch_all` + built the
  whole overlay + a single terminal `checkpoint`.
- **Test:** triggered `fuzzy-sync` via the MCP `trigger_cron` tool while sampling RSS.
- **Result:** RSS climbed **1.48 → 12.34 GB in ~2 min and kept climbing** — in-use,
  not reclaimed by the mimalloc purge env (`MIMALLOC_PURGE_DELAY=0`) I had set, nor
  by disabling libdictenstein eviction (`max_disk_bytes=0`). **Confirmed: in-use
  overlay, built whole in RAM.**

## H4: one pathological project dominates → CONFIRMED

- **Test:** `du` the trie dir + count rows per project.
- **Result:** the fuzzy dir is **103 GB**; a single trie —
  `symbols/default-p375739/symbols.artrie` — is **11.5 GB** (+ its 5.3 GB paths trie).
  Project `default` (id 375739) is a **catch-all holding 22,541 files across 62
  workspace directories** (worktree/branch dirs: `f1r3node-*`, `mettail-*`,
  `MeTTa-Compiler-PR-63`, …). Building that 11.5 GB trie in RAM is the balloon.
- **Why they land in `default`:** `scanner::find_project_root` only consults the
  in-memory `project_roots` DashMap; on a miss, every file collapses into one
  synthetic `default` project (`src/embed/pool.rs:986`). Git-less dirs, transient
  worktrees, and walk/resolve races all merge there.

## The fix (commit `f5b13de`)

- **A1 + A2 — memory-bound the rebuild (the general fix):** keyset-paginate the
  source + checkpoint per page (`src/fuzzy/sync.rs`), with heap eviction primed
  BEFORE the rebuild (`prime_eviction`), gated by new `[fuzzy] resident_budget_bytes`
  + `checkpoint_every_rows`. Peak RAM ≈ one page + the budget, regardless of trie
  size.
- **BLOCKER — libdictenstein char-eviction corruption:** activating
  `resident_budget_bytes` currently corrupts the trie ("char v2 sequential child
  mismatch"): the resident-budget eviction evicts individual cold nodes, scattering
  a *sequential-sibling* parent's children across arenas, which the next checkpoint's
  contiguity invariant rejects. Root-caused + reported in
  `../libdictenstein-char-resident-eviction-corruption-bug.md`. A1 is therefore
  shipped **DORMANT** (`resident_budget_bytes = 0`) with an `#[ignore]`d gate test;
  the libdictenstein agent owns the fix (user-coordinated).
- **Independent wins shipped live-ready:** `client_file_events` + exited
  `mcp_clients` retention (the events table had NO retention → 31 M rows / 9.7 GB);
  `begin_heavy` statement-timeout lifts across long-running crons; the
  log-broadcaster peer-accumulation leak fix.

## Still open (gated, not abandoned)

1. **A1 activation** — UNBLOCKED (2026-07-08): libdictenstein fixed the char-eviction
   corruption (root cause was an arena-space/block-space off-by-one in
   `check_sequential_char_children`, not node scattering; a companion "dirty-skip"
   fix also bounds per-checkpoint DISK growth to `O(dirty nodes)`). Activating:
   `[fuzzy] resident_budget_bytes` 0 → 768 MiB (lib guidance: keep it modest) +
   un-`#[ignore]` the `resident_budget_eviction_reclaims_and_preserves_terms` gate
   (now passes). A1 becomes the PRIMARY RAM bound (any trie builds in ~the budget).
   skip-oversize stays as defense-in-depth — it also guards the lib's "reopen
   eager-loads the full image" caveat (a huge trie is a RAM risk at query/restart
   even when A1 bounded its build) — and C-layer keeps tries small at the source.
2. **C-layer (default-bloat root cause)** — `scanner::find_project_root` FS git-root
   fallback + per-top-level-dir projects + worktree→main canonicalization
   (`git_common_dir`, reusing `pick_main_worktree_ids`). Designed; gated on disk
   headroom for the validation build.
3. **Disk** — `target/` cleaned (50 GB → 125 G free). The 103 GB fuzzy tree and a
   full deploy/reindex are gated on disk headroom + the above.

Interim safety: cgroup `MemoryMax=24G` + `fuzzy_sync_interval_secs=86400` (raised
from 30 min) prevent a system OOM while A1 is dormant.

## H5 (CORRECTED, decisive): stale TRIE bloat + reopen-eager-load — CONFIRMED

After deploying A1 + skip-oversize + C-layer, a live triggered fuzzy-sync STILL
ballooned to ~13 GB (though it plateaued < the 24 GB backstop, no system OOM). The
diagnosis of H3/H4 was **incomplete**: the balloon is neither the build overlay (A1
bounds it) nor the current source size (skip-oversize checks it).

- **Test:** `/proc/PID/smaps_rollup` → 13.6 GB **anonymous heap**; `/proc/PID/maps` →
  one `.artrie` mmap of **11.28 GB** (the `default` symbols trie) open mid-sync.
  `SELECT count(*)` over `default`'s current `file_symbols` = **208,625**.
- **Result (CONFIRMED):** the `default` trie is **11.5 GB on disk with only 208 K
  current symbols** — it is **stale-bloated**. `rebuild_symbols`/`paths`/`commits`
  *upsert* current terms but NEVER remove deleted ones, so as a project's source
  shrinks (file cleanup: millions → 208 K) the trie retains every old term. Each
  fuzzy-sync `open_or_create`s that bloated trie, and libdictenstein's reopen
  **eager-loads the full ~11 GB image into heap** — the balloon. This is why A1
  (build-eviction) and skip-oversize (source-count) both failed to stop it.

**Fix applied (decisive):** wiped `$data_dir/fuzzy` (103 GB, mostly stale bloat) →
reclaimed ~100 GB → tries rebuild fresh from the current (small) sources. **Live
re-verified: fuzzy-sync RSS now plateaus at ~1.5–2.0 GB with a clean sawtooth**
(tries free between iterations when not stale-huge) — down from 48 GB / 13 GB. The
committed fixes (skip-oversize, A1, C-layer, backstop) are complementary safety
nets; the reset was decisive.

## Durable fix — IMPLEMENTED (2026-07-09, uncommitted on `main`)

The one-time wipe is now backed by a code change so the tries cannot re-accumulate
deleted terms. `run_fuzzy_sync` (`src/cron/fuzzy_sync.rs`) rebuilds each trie from a
CLEAN on-disk slate, gated on a per-trie data-change check that mirrors
`memory_graph_refresh`:

- **Rebuild-fresh.** Before a trie is reopened it is reset to empty via
  `reset_trie_on_disk`: per-project tries live in a dedicated dir
  (`.../{kind}/{key}/`) → `remove_dir_all` the parent; the two workspace-global
  tries (durable-mandates, concepts) share `$data_dir/fuzzy/`, so only their own
  stem-matched files (`{stem}.artrie`/`.wal`/compaction siblings) are removed —
  never the sibling trie, the `.format_version` sentinel, or the shared
  `wal_pending`/`wal_archive` dirs. `open_or_create` then takes the `create` path
  (no WAL replay), so the trie holds ONLY the live source.
- **Per-trie data-change gate.** A cheap `count(*):max(id)` fingerprint over each
  trie's PG source (via `begin_heavy`) is compared to a `pgmcp_metadata` watermark
  (`fuzzy_sync:{kind}[:{project_id}]`). Unchanged-within-window tries are skipped
  (`FuzzySyncReport.skipped_unchanged`), so the job does not rewrite every trie
  every run. `count(*)` moving on DELETE is what catches a *shrinking* source (the
  stale-accumulation trigger). Backstop knob `[cron] fuzzy_sync_max_staleness_secs`
  (default 7 days) forces an occasional rebuild of a low-churn trie. The watermark
  is stamped ONLY after a successful rebuild, so a failure re-attempts next run.
- **Reader-safety.** We accept the brief rebuild window (no temp+swap): an existing
  `FuzzyCache` reader keeps its mmap of the old inode; only a reader that OPENS
  during the sub-second gated rebuild sees an empty trie briefly — acceptable for
  the best-effort fuzzy leg. The cache's mtime check re-opens the new inode after.

Pure gate/reset logic is unit-tested (`is_unchanged`, `watermark_key`,
`reset_trie_on_disk` per-project + shared-dir) in `src/cron/fuzzy_sync.rs`. Verified
via `cargo clippy --bin pgmcp --all-targets -D warnings` (clean) + the fuzzy_sync
unit tests (14 pass). Full `scripts/verify.sh` + deploy pending (operator-run).
