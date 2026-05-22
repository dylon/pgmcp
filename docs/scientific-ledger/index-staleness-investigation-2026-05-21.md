# Index Staleness Investigation & Latent-Bug Fixes — 2026-05-21

## Context

A user-supplied tool `lling-llang` reported:

> the pgmcp index is stale (225 modified files vs. indexed snapshot)

for the `pgmcp` project. The user asked why some indexed projects'
indices weren't being updated. The investigation involved two parallel
Explore agents (current-state snapshot + indexing-pipeline mapping)
plus a third targeted at heavy-cron observability.

## Hypothesis chain

**H1.** pgmcp's index is genuinely stale for the named project.
*Falsified.* `mcp__pgmcp__file_info` on every sampled modified file
(`src/mcp/server.rs`, `src/logging.rs`, `README.md`, plus 10 untracked
files) showed `indexed_at` within ~1 second of filesystem mtime.

**H2.** `lling-llang` compares against an older snapshot, not pgmcp's
live state. *Supported.* Live `git status --porcelain | wc -l` = 200;
lling-llang claims 225. Unit-mismatch (porcelain `?? src/a2a/` is one
line but the directory holds 9+ indexed files) compounds the
discrepancy. `Cargo.lock` is correctly excluded by
`indexer.exclude_patterns = ["*.lock"]` but a comparator that doesn't
honor exclude_patterns would flag it as missing.

**Verdict on the user's question:** False alarm. pgmcp is current.

## Latent bugs surfaced during the investigation

Three real bugs were uncovered. The user requested principled
resolution for all three.

### Bug A — `projects.last_scanned_at` never written

`src/db/queries.rs:341` defined `update_project_scanned`. The trait at
`src/db/client.rs:80` exposed it. **No production call site existed.**
Every one of the 76 indexed projects showed `last_scanned_at: NULL`.
Any external tool reading this column to gauge freshness would report
all projects as "never scanned" forever.

### Bug B — Synthetic roots not live-watched

`~/.claude/`, `~/.codex/`, `~/Papers/`, `~/Documents/` were scanned
once at daemon startup via special-case scanner entry points
(`scanner.rs:404` `scan_claude_dir`, `:484` `scan_codex_dir`, plus
Papers/Documents). `src/indexer/watcher.rs:48-118` `start_watching`
iterated only `config.workspace.paths`. Synthetic roots never received
live inotify events — edits drifted until daemon restart.

### Bug C — Heavy crons silently no-op without distinguishing reasons

`cron_executions: 363` recorded but per-cron work-counters (`topic_scans`,
`similarity_scans`, `graph_build_runs`, `symbol_extraction_runs`,
`function_metrics_runs`) all read zero. Each of the seven heavy-cron
closures in `src/cron/scheduler.rs:740-1230` had three "successful
no-op" gates (`is_at_least(Ready)`, `first_seen.elapsed() < cooldown`,
`heavy_cron_lock.try_lock()`) that returned without recording WHY they
skipped. The scheduler's outer closure always returned `true`, so the
default outcome was `Ok` — making "ran fully", "skipped at gate", and
"ran but found nothing to do" structurally indistinguishable from the
outside.

## Principled resolutions

### Bug A

1. Extended `INSERT INTO projects ... ON CONFLICT (path) DO UPDATE` in
   `queries::upsert_project` to set `last_scanned_at = NOW()` on both
   the insert and the conflict branch (`src/db/queries.rs:18-50`). Per-
   file processing through `embed/pool.rs` now bumps `last_scanned_at`
   automatically.
2. Added `update_projects_scanned_by_workspace(workspace_path)`
   (`queries.rs` + trait at `db/client.rs`) for the no-files-changed
   path. Called from:
   - `event_processor::start_indexing` after the initial scan completes
     (one bulk update per workspace + per synthetic root).
   - `event_processor::rescan_workspace` after each rescan completes.
3. Added `StatsTracker::last_scanned_writes: AtomicU64` counter,
   incremented per row updated, exposed in the `index_stats` snapshot.
4. Mock `DbClient` in `pgmcp-testing/src/mocks.rs` extended with the
   new trait method.

### Bug B

1. Added `effective_workspace_paths(&Config, &SyntheticRoots) ->
   Vec<String>` to `src/indexer/scanner.rs`. Returns the union of
   `config.workspace.paths` with any synthetic roots that exist on
   disk (`~/.claude`, `~/.codex`, `~/Papers`, `~/Documents`).
2. `SyntheticRoots::present()` helper that iterates only existing
   roots — used by the new union helper and could be used by future
   logging.
3. `event_processor::start_indexing` constructs a `SyntheticRoots`
   handle and passes the unified path list to `start_watching`. The
   inotify watcher now covers synthetic roots with the same
   recursive-watch + overflow-recovery semantics as configured
   workspaces.
4. Reinit (overflow re-arm) path inherits the unified set automatically:
   the watcher's `workspaces_for_cb` captures whatever was passed to
   `start_watching`, so re-arm covers synthetic roots too.

### Bug C

1. Introduced `SkipReason::{PhaseGate, Cooldown, LockBusy}` and
   extended `CronJobOutcome` from `Ok | Panicked` to
   `Ok | NoOp | Skipped(SkipReason) | Panicked`
   (`src/stats/tracker.rs:13-90`).
2. New `heavy_gate_or_skip!` macro in `src/cron/scheduler.rs:54-100`
   replaces the three-gate body in each of the seven heavy-cron
   closures. Each gate records the exact `SkipReason` via
   `stats.record_cron_outcome(...)` before returning.
3. Each heavy-cron body file (`graph_analysis.rs`, `similarity.rs`,
   `symbol_extraction.rs`, `function_metrics.rs`, `call_graph.rs`,
   `topic_clustering.rs`) had its `<job>_runs` counter promoted to
   top-of-body. The counter now means "the body reached its
   work-eligible state" rather than "the body completed successfully".
4. Added `<job>_noop_returns` counters for each heavy cron, incremented
   at the empty-data path (`projects.is_empty()`, `max_chunk_id == 0`,
   `chunk_count == 0`, no git-enabled projects). The new
   `CronJobOutcome::NoOp` variant is recorded in tandem.
5. Added `git_history_runs` + `git_history_noop_returns` counters
   (previously git-history-index had no top-level counter — only
   per-commit `git_commits_indexed`).
6. Added `DaemonLifecycle::phase_started_at_ms()` +
   `ms_in_current_phase()` so the `Cooldown` skip-log can include
   "in Ready for X ms" context.
7. All new counters exposed in the `index_stats` snapshot.

## Verification (Bug-specific gates)

### Bug A

```bash
pgmcp daemon &
# Edit any file in any indexed project.
mcp__pgmcp__list_projects | jq '.[] | select(.name == "pgmcp") | .last_scanned_at'
# Expect: ISO-8601 timestamp, not null.

pgmcp context | grep "Last scanned:"
# Expect: recent timestamp, not "never".

mcp__pgmcp__index_stats | jq .last_scanned_writes
# Expect: > 0 and increasing across scan cycles.
```

### Bug B

```bash
echo "test line $(date)" >> ~/.claude/projects/<session-dir>/<file>.jsonl
sleep 2
mcp__pgmcp__file_info path=<file>
# Expect: indexed_at within ~1 s of filesystem mtime.
```

### Bug C

```bash
# Pre-fix: zeros despite cron_executions in the hundreds.
mcp__pgmcp__index_stats | jq '{
  cron_executions, graph_build_runs, graph_build_noop_returns,
  similarity_scans, similarity_noop_returns,
  symbol_extraction_runs, symbol_extraction_noop_returns,
  function_metrics_runs, function_metrics_noop_returns,
  call_graph_runs, call_graph_noop_returns,
  topic_scans, topic_clustering_noop_returns,
  git_history_runs, git_history_noop_returns,
  last_cron_outcomes
}'
# Expect: runs counters increment over time; last_cron_outcomes
# shows the actual outcome string ("ok", "no_op", "skipped:phase_gate",
# "skipped:cooldown", "skipped:lock_busy", "panicked").
```

## Final gate

`./scripts/verify.sh` — all 8 gates pass.

## Risks / known limitations

- **Bug A:** zero risk. No schema change (column existed); helper
  existed; we wire two call sites + per-row trigger via upsert.
- **Bug B:** inotify watch count grows by the recursive subtree size of
  whichever synthetic roots exist on the host. On a default kernel
  (`fs.inotify.max_user_watches = 524288`), unlikely to hit limits.
  Existing overflow re-arm covers the case if it does.
- **Bug C:** behavior-preserving signal change. No cron's actual
  execution shifts; only the observability of skip-vs-no-op-vs-done.
  The `CronJobOutcome` enum extension is backwards-incompatible at the
  pattern-match level — the compiler enforces exhaustiveness, so any
  missed match arm is a build error rather than a runtime surprise.

## Out-of-scope (documented in plan)

H1, H3-H5, H7-H14 from the pipeline-agent diagnostic — theoretical
staleness vectors that did not contribute to the observed report. See
the original plan file at
`~/.claude/plans/how-can-pgmcp-be-smooth-cherny.md` for the full
hypothesis list.

`lling-llang` itself: external to pgmcp; the audit recommendation
remains in the plan file should the user want to follow up.
