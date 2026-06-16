# ADR-019: Index freshness — `last_verified_at`, reconcile backstop, bounded failure retry

- Status: Accepted (implemented)
- Date: 2026-06-16
- Supersedes/relates: closes the open follow-ups from the 2026-05-21 staleness
  investigation (`docs/scientific-ledger/index-staleness-investigation-2026-05-21.md`);
  complements ADR-013 (disk-fallback symbol extraction), ADR-015 (DB resilience),
  and ADR-018 (durable cron-run history). Migrations `v41` + `v42`.

## Context

The triggering question: *"File indices tend to get stale when `grep` is used via
pgmcp's MCP API — are file-change events being subscribed to and handled in a
timely manner to update the indices/embeddings?"*

### What the investigation found

pgmcp's freshness pipeline is **event-driven and timely**. A live `notify` v7
(inotify) recursive watcher (`src/indexer/watcher.rs`) covers every workspace plus
the synthetic roots (`~/.claude`, `~/.codex`, `~/Papers`, `~/Documents`; the Bug-B
fix from the 2026-05-21 investigation), debounces per path for `[indexer]
debounce_ms` (default 300 ms), and submits changed files to the embed pool
(`src/embed/pool.rs::process_index_file_task`) for re-chunk + re-embed + upsert.
Measured end-to-end latency on a live edit: **~381 ms** (`src/lib.rs`, with
`file_info.modified_at` = the file's mtime and `indexed_at` = the row write 381 ms
later). Health counters at the time: 40 354 watcher events received, **0** inotify
overflows, **0** watcher errors, **0** embed errors.

So why the *perception* of staleness? Two distinct causes, one perceptual and one
real-but-dormant.

#### Cause 1 — the false-staleness signal (the dominant complaint)

`indexed_files.indexed_at` and `modified_at` freeze at the last **content** change.
The indexer skips unchanged files in two stages:

```text
            ┌─────────────────────────────────────────────────────────────┐
            │  scanner / rescan walk         (src/indexer/event_processor) │
            │                                                              │
   path ───▶│  Level-1 metadata skip   fs_size == db.size_bytes            │
            │     (stat only, NO read)  ∧ fs_mtime <= db.modified_at  ──┐  │
            │                                                          │skip
            └──────────────────────────────────────────────────────────┼──┘
                                    │ (changed or new)                  │
                                    ▼                                   │
            ┌─────────────────────────────────────────────────────────────┐
            │  embed pool worker             (src/embed/pool.rs)        │  │
            │  Level-2 content-hash skip   xxh3_64(content) == stored ─┐│  │
            │     (read + hash, NO re-embed)                          skip │
            └──────────────────────────────────────────────────────────┼┼──┘
                                    │ (hash differs)                    ││
                                    ▼                                   ▼▼
                          full re-chunk + re-embed + upsert        (no DB write)
```

In a multi-branch workspace a `git checkout` / `rebase` / `stash pop` rewrites a
file to **byte-identical content but a fresh mtime**. The Level-2 content-hash skip
correctly avoids re-embedding — but it writes nothing, so `indexed_at` stays at the
*old* content-change time. Any consumer that compares `file_info.indexed_at`
against the file's current disk mtime then reports a **false** "stale": the bytes
are current, the timestamp looks old.

This is not hypothetical. It fooled a sub-agent during this very investigation
(claiming `event_processor.rs` was stale when `file_info` showed `size`/`lines`
matching disk exactly), and it fooled the original 2026-05-21 investigation (the
"225 modified files" false alarm). The index had no timestamp answering the
question consumers actually ask — *"when did you last confirm this row matches
disk?"* — only *"when did its content last change?"*

`grep` is the canary because it serves `file_chunks.content` straight from
PostgreSQL with **no disk fallback** (unlike `read_file`, ADR-013), so any genuine
DB staleness — or any tool that *infers* staleness from `indexed_at` — surfaces
through `grep` first.

#### Cause 2 — no backstop for genuinely missed events (real, dormant)

Freshness relied **entirely** on the live watcher. When an inotify event is *not
delivered* — queue overflow past the re-arm, an editor that saves atomically by
writing a temp file and renaming it with preserved metadata, the daemon being down
during an edit, or the ADR-015 intake-gate-closed window during a DB outage —
nothing re-checks that file until the **next daemon restart**. The only
filesystem re-walks were: the one-shot startup scan, a config workspace-path
addition, and the inotify-overflow `Reinit`. The `stale-cleanup` cron handles
*deletions* only; `integrity-check` only GCs `content_hash IS NULL` rows. Neither
re-stats existing files for modification.

#### Cause 3 — opaque, unbounded indexing failures

`index_stats.files_failed` was a single opaque counter (402 at investigation time)
with no record of *which* files or *why*, and no retry policy. Content-intrinsic
failures (a non-UTF-8 file with a code extension; a corrupt or oversized document
that fails / times out / OOMs extraction) would be re-read and re-fail on every
single re-walk — re-running `pandoc` on a corrupt 50 MiB PDF indefinitely.

### Constraint

The Level-1 metadata skip is a **hard performance mandate**
(`feedback_rescan_metadata_skip.md`): it must stay stat-only with **no per-file DB
write**, or a re-walk self-DoSes on 21 k+ files. Every fix below honors this — all
freshness writes are bulk (one `UPDATE` per scan) or fire only on an actual event.

## Decision

Three complementary changes, none of which touch the Level-1 skip's hot path.

### 1. `last_verified_at` — the false-staleness-proof freshness signal (v41)

Add `indexed_files.last_verified_at TIMESTAMPTZ` (migration
`v41_indexed_files_last_verified.rs`). Semantics: *the wall-clock time the indexer
last confirmed this row matches disk.* It advances on **every** path that
establishes the row is authoritative:

| Event | Where | How |
|---|---|---|
| Level-1 metadata skip (unchanged) | `event_processor.rs` scan + `rescan_workspace` | bulk `mark_files_verified(&skipped_paths)` after the walk — **one `UPDATE … WHERE path = ANY($1)`** for the whole skipped set |
| Level-2 content-hash skip (git-touched) | `pool.rs` (3 skip exits) | single-row `mark_file_verified(path)` — fires only on an actual event |
| Full re-index / rename / duplicate | `replace_indexed_file` / `update_file_path_in_place` / `insert_duplicate_file` | `last_verified_at = NOW()` folded into the same write |

Passing the **exact** skipped-path set (not a `path LIKE '/ws/%'` prefix) is
deliberate: a prefix `UPDATE` would wrongly mark rows the walk did not see this
pass (an in-flight re-embed; a child-`.pgmcp.toml`-excluded file). The scan already
materializes the skipped set, so `path = ANY($1)` is one index-backed round-trip
keyed on `UNIQUE(path)`.

Invariant: `last_verified_at >= indexed_at` always (every content write stamps
both). A git-touched-but-unchanged file now reads `last_verified_at = <recent>`
while `indexed_at` stays old — exactly inverting the false signal.

**Surfacing.** `file_info` gains `last_verified_at`, a live `disk_mtime` (one
`stat` at call time), and a derived `verified_current = last_verified_at >=
disk_mtime` — a caller reads `verified_current: true` and stops hand-rolling the
`indexed_at`-vs-mtime comparison that produced the false positive. `orient`'s
`health` block gains the corpus `last_verified_at` (`MAX` over the project) and
`index_reconcile_stale` (newest verify older than 2× the reconcile interval), the
freshness analog of `graph_stale` / `topics_stale`.

### 2. Reconcile-backstop cron — self-heal missed events (`src/cron/index_reconcile.rs`)

A periodic cron sends one `WatcherCommand::Rescan(path)` per workspace root (config
paths ∪ synthetic roots, via `effective_workspace_paths`). The existing
watcher-command thread serializes them through `rescan_workspace`, which applies
the **same Level-1 stat-only skip** (and the bounded-failure gate, §3) — so a
reconcile pass is `O(stat)` over the corpus plus the cost of the usually-empty
changed set. Missed live events self-heal within one interval instead of waiting
for a restart.

```text
   every [cron] index_reconcile_interval_secs (default 1800 = 30 min)
        │
        ▼
   index-reconcile cron  ──(Ready-gated; PhaseGate skip recorded if not Ready)──▶
        │  run_or_log: try_send WatcherCommand::Rescan(ws) for each workspace
        ▼
   watcher-command thread  ──serialized──▶ rescan_workspace(ws)
        │                                    ├─ Level-1 skip (unchanged) ─▶ bulk mark_files_verified
        │                                    ├─ Level-0 bounded-failure skip (unchanged bad file)
        │                                    └─ changed/new ─▶ embed pool (full pipeline)
        ▼
   cron_run_history row (ADR-018) + stats.index_reconcile_runs
```

**Wiring.** Registered inline in `daemon.rs` (like the a2a / csm / security crons),
because it needs the `watcher_cmd_tx` created there — not in
`schedule_maintenance_jobs`. Ready-gated (no point reconciling a half-built index
against the startup scan), shutdown-gated, and ledgered via
`cron::history::spawn_recorded`. Restart-survival initial delay
(`restart_initial_delay_ms`, ADR-018). `index_reconcile_interval_secs = 0`
disables. It is deliberately **separate** from `integrity-check` (24 h NULL-hash
GC) and `stale-cleanup` (1 h deletion sweep); folding the FS re-walk into the 24 h
cron would make missed-event recovery take up to a day.

### 3. Bounded failure retry — `index_failures` ledger (v42)

A small `index_failures` table (migration `v42_index_failures.rs`) keyed on `path`,
recording **content-intrinsic** failures only — the closed `FailureKind` vocabulary
(`src/embed/failure_kind.rs`, ADR-003 `TEXT` + `CHECK` idiom): `not_utf8`,
`doc_extract_failed`, `doc_extract_timeout`, `doc_extract_oom`. Transient
infrastructure failures (DB upsert/replace timeouts) are **not** ledgered — they
self-heal on the next reconcile once the infra recovers, and recording them would
mean writing to a possibly-down database; they keep incrementing `files_failed`.

The scanner loads the bounded set once per walk (like `metadata_map`) and applies a
**Level-0 gate**: skip re-submitting a file whose `failure_count >= [indexer]
max_index_retries` (default 5) **and whose mtime has not advanced past
`last_failed_at`**. Editing the file (mtime advances) lifts the bound and earns a
fresh attempt; a successful (re)index clears the row in-transaction inside
`replace_indexed_file` / `insert_duplicate_file` / `update_file_path_in_place`.

```text
   file submitted to scanner
        │
        ▼
   in index_failures with count >= max_index_retries?
        │ no ──────────────────────────────▶ proceed (Level-1 / Level-2 as usual)
        │ yes
        ▼
   fs_mtime > last_failed_at ?   (was it edited since we last failed?)
        │ yes ─▶ proceed (fresh attempt; success clears the row, failure bumps count)
        │ no  ─▶ SKIP  (bounded: don't re-run extraction on an unchanged bad file)
```

`index_stats` surfaces a `failure_kind` breakdown, turning the opaque
`files_failed` counter into an actionable list.

## Schema

```sql
-- v41
ALTER TABLE indexed_files ADD COLUMN IF NOT EXISTS last_verified_at TIMESTAMPTZ;

-- v42
CREATE TABLE IF NOT EXISTS index_failures (
    path            TEXT        PRIMARY KEY,
    failure_kind    TEXT        NOT NULL,   -- CHECK ∈ FailureKind::sql_in_list()
    failure_count   INTEGER     NOT NULL DEFAULT 1,
    first_failed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_failed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error      TEXT
);
CREATE INDEX IF NOT EXISTS ix_index_failures_count ON index_failures (failure_count);
```

New `[indexer] max_index_retries` (default 5) and `[cron]
index_reconcile_interval_secs` (default 1800). New `StatsTracker` counters
`last_verified_writes`, `files_bounded_skipped`, `index_reconcile_runs`.

## Correctness properties

1. **Level-1 mandate honored.** The stat-only skip branch is unchanged; all
   freshness writes are bulk-per-scan (`mark_files_verified`) or per-event
   (`mark_file_verified`). No per-file DB write was added to the skip path.
2. **`last_verified_at >= indexed_at`.** Every content-writing path stamps both;
   verify-only paths stamp only `last_verified_at` (which is `>= indexed_at` by
   construction since the content was last written no later than now).
3. **Recovered files leave the ledger.** Clear-on-success is in-transaction in the
   three canonical writers, so a `failure_count`-bounded file cannot stay bounded
   after it indexes — and the Level-0 gate's mtime-advance check additionally
   un-bounds an edited file before re-attempt.
4. **No infinite extraction loop.** A permanently-bad, unedited file is skipped
   after `max_index_retries`; only an edit (mtime advance) re-arms it.
5. **Reconcile is cheap and bounded.** It reuses `rescan_workspace`'s Level-1
   skip, so it reads only genuinely-changed files; the watcher-command thread
   serializes the per-workspace walks (natural throttle).

## Consequences

- **Positive.** The recurring false-staleness misread (which fooled humans and
  agents twice) is eliminated at the source — consumers compare `last_verified_at`
  / `verified_current`, not `indexed_at`. Missed events self-heal within one
  reconcile interval instead of requiring a daemon restart. The opaque
  `files_failed` count becomes a queryable, bounded `failure_kind` breakdown.
- **Cost.** One bulk `UPDATE` per scan/reconcile; one extra `stat` per file in
  `file_info`; one single-row `UPDATE` per git-touch event (negligible at the
  observed event rate); a full corpus stat-walk every 30 min (cheap — only changed
  files are read). `index_failures` is bounded by the number of permanently-bad
  files (hundreds at most).
- **Not done — manual `trigger_cron` for `index-reconcile`.** Triggering it from
  the MCP tool would require plumbing `watcher_cmd_tx` into `SystemContext`, out of
  proportion to the value. The cron runs on its schedule; the interval is tunable
  (e.g. lower it transiently) for testing. This breaks no coverage gate — the cron
  is not a dispatched tool. Recorded here so the omission is intentional, not an
  oversight.

## Verification

- `cargo check` (both crates, all targets), `cargo fmt --check`, `cargo clippy
  --workspace --all-targets -D warnings` — clean.
- Unit: `FailureKind` vocab golden, `run_or_log` channel behavior, config-default
  pins, `v41`/`v42` `step_version_is_stable`.
- Real-Postgres integration (`index_freshness_integration.rs`): `mark_files_verified`
  advances `last_verified_at` without touching `indexed_at`; `index_failures`
  UPSERT/threshold/clear lifecycle; the v42 CHECK rejects an unknown `failure_kind`.
- `scripts/verify.sh` (full release build + GPU smoke) is the final gate.
