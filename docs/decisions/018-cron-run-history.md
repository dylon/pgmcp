# ADR-018: Durable cron-run history (restart-survival + intrinsics + failures)

- Status: Accepted (implemented)
- Date: 2026-06-15
- Supersedes/relates: complements the in-memory `last_cron_outcomes` snapshot
  (`src/stats/tracker/outcomes.rs`), surfaced by `index_stats`.

## Context

pgmcp's cron scheduler (`src/cron/scheduler.rs`) was **100% in-memory**. The
`CronStateMachine` keeps a `BinaryHeap<ScheduledTask>` keyed by
`scheduled_time_ms`; on each run it reschedules at `now_ms() + interval`.
`record_cron_outcome` wrote only the *latest* outcome per job to a `DashMap`.
Three consequences:

1. **A daemon restart reset every cron's timer** — elapsed time since the last
   run was lost; every schedule restarted from a fresh stagger.
2. **The first post-restart run of a heavy cron slipped ~one full interval.**
   The first tick fired at the startup stagger (`staggered_initial_delay_ms`,
   1s–10min), but the `heavy_gate_or_skip!` ready-relative cooldown
   (`ready_delay_topic_secs = 3600` → 1h) skipped it, and the next recurring tick
   was +interval later. For `topic-clustering` (`topic_scan_interval_secs =
   43200` → 12h) the topic model was **not rebuilt for ~12h after any restart** —
   the live symptom that motivated this work (topics frozen at the degenerate
   2026-05-19/20 stopword state even after the ADR-017 graph-engine fix shipped).
3. **No record of intrinsics or failures** survived a restart, nor was history
   queryable.

**Goal (user's words):** "cron runs should be recorded in the database, including
their intrinsics and any failures they encounter," and on startup compute each
job's next-due time from its last persisted successful completion instead of a
fresh timer.

## Decision

A durable append-only ledger `cron_run_history` (v40), filled by a non-blocking
channel writer, plus restart-survival scheduling and a honor-settle cooldown.

### 1. Schema — `cron_run_history` (migration `v40_cron_run_history.rs`)

One row per run (scheduled / manual / startup): `job_name`, `trigger_source`,
`outcome`, `skip_reason`, `error_detail`, `project`, `started_at`,
`completed_at`, `duration_ms`, `rss_mb_{start,end,delta}`,
`threads_{start,end,delta}`, `counters JSONB`. Three closed-vocabulary CHECKs
(`trigger_source` / `outcome` / `skip_reason`) installed via the stamp-aware
`install_check` (ADR-003 idiom) so a later enum change re-applies via DROP+ADD.
A **partial index** `WHERE outcome = 'ok'` serves the restart hot path; a
`(job_name, completed_at DESC)` index serves the tool; a `(completed_at)` index
serves the retention sweep.

### 2. Vocabularies — `src/cron/history/vocab.rs`

`CronTriggerSource { Scheduled, Manual, Startup }` and `CronOutcome { Ok, NoOp,
Skipped, Panicked, Failed }`, each an `ALL`/`as_str`/`parse`/`sql_in_list` closed
enum with golden tests (the `severity.rs` idiom). `CronOutcome` is a **separate
persistence-layer enum**, deliberately distinct from the in-memory
`CronJobOutcome` (whose `as_str()` flattens `Skipped(reason)` for the existing
`index_stats` JSON + goldens). `Failed` (an internal top-level `Err`) has no
in-memory analogue; `From<CronJobOutcome> for (CronOutcome, Option<SkipReason>)`
maps the gate path. The six `SkipReason` variants are enumerated here (guarded by
a compile-time wildcard-free exhaustiveness match) without modifying
`outcomes.rs`.

### 3. Writer + guard — `src/cron/history/mod.rs`

Mirrors the proven `mcp_tool_calls` telemetry writer
(`src/stats/telemetry_writer.rs`): a bounded tokio `mpsc` channel drained by one
tokio task that batch-INSERTs via `UNNEST`, with `try_send` drop-on-overflow
counted by `StatsTracker::cron_history_writes_dropped` (surfaced in
`index_stats`), so the scheduler / work-pool threads **never block**. Graceful
shutdown flushes via the daemon's `CancellationToken` (the writer's `JoinHandle`
is awaited in `run_server`'s shutdown path).

`CronRunGuard` is the RAII recorder (mirrors `HeavyCronFlag`): constructed at the
top of a cron body, it captures start intrinsics (`crate::stats::rss` exact RSS +
`/proc/self/task` thread count) and on `Drop` writes exactly one row — defaulting
to `Panicked` if the body unwound without a finisher (`ok`/`ok_with(counters)`/
`noop`/`fail`/`skipped`). `try_send` is callable from any thread, so the guard's
`Drop` works on the WorkPool threads. `spawn_recorded(rt, hist, job, fut)` is the
uniform recorder for the `rt.spawn(async { run_or_log(...) })` light-cron pattern.

### 4. Restart-survival (§5) — `restart_initial_delay_ms`

`daemon.rs` reads `last_successful_completions(pool)` at startup into a
`HashMap<job, last_ok>` and passes it to `schedule_maintenance_jobs`, where a
local `initial_delay(job, interval_ms)` closure computes the first-tick delay via
the pure `restart_initial_delay_ms(job, interval_ms, last_ok_ms, now_ms)`:
overdue / unknown / clock-skew fall back to the anti-herd
`staggered_initial_delay_ms`; a recent success waits `max(next_due − now,
stagger)`. Unit-tested (no DB).

**Scope nuance (a deliberate refinement of the original plan).** Restart-survival
replaces the `staggered_initial_delay_ms(...)` call sites (a clean win). It does
**not** replace the *deliberate fixed staggers* that encode intent —
`target-cleanup`'s fixed 600s ("surface a dry-run manifest soon after every
restart", which restart-survival would defer to the next weekly due-time), the
sub-10s `stats-aggregation`/`stale-cleanup` cadences, and the daemon.rs ontology
sequencing staggers (build→integrate→reason). Those keep their fixed delays;
**every** cron still gets run-recording (the core ask), regardless of its delay
policy.

### 5. Honor-settle cooldown (§6) — the 12h-slip fix

The ready-relative cooldown moved **out** of the in-pool gate and **up** into the
recurring closure (`heavy_cron_tick`, on the scheduler thread). On a settle-skip
it records a `Cooldown` skip and schedules a **one-shot retry at
~cooldown-expiry** (`schedule_heavy_retry`, a self-re-arming trampoline measured
against `lc.ms_in_current_phase()`), instead of letting the skip slip the run a
full interval. The retry is bounded: `ms_in_current_phase` grows monotonically
while Ready, and a phase regression (e.g. a mid-run reindex) simply re-waits
Ready. Net effect: an overdue `topic-clustering` runs **~1h after Ready** instead
of ~12h, without competing with boot-time scanning/embedding.

The in-pool `try_heavy_gate` fn (which replaced the `heavy_gate_or_skip!` macro —
a fn lets `register_heavy_cron` pass a runtime `job: &'static str` and use
structured logging instead of `concat!`) keeps the gates that must hold inside
the pool task: PhaseGate, Shutdown, DbDown, DiskPressure, LockBusy — each now
also recording to `cron_run_history` via `record_skip`.

### 6. `register_heavy_cron` — one place for the subtle logic

The 15 heavy crons were unified behind `register_heavy_cron(handle, lifecycle,
cron_pool, lock, stats, hist, job, initial_delay, interval, cooldown, body)`. It
builds the re-callable `run_once` (submit body to pool, behind `try_heavy_gate` +
`HeavyCronFlag` + a `CronRunGuard`), wires `heavy_cron_tick` + the §6 retry, and
schedules the recurring tick. Each cron shrank to a `body: Fn(&mut CronRunGuard)`
closure that does its work and records its outcome — eliminating ~15× duplicated
gate/flag/cooldown/RSS-logging boilerplate (the guard's intrinsics replace the
per-body manual RSS logging). `quality-history`/`tool-policy-refresh` pass their
own lock + a 120s settle; the rest share `heavy_cron_lock`.

### 7. Coverage

- **Heavy crons (15):** all via `register_heavy_cron` (recording + §5 + §6).
- **Light crons (~24, scheduler.rs inline + daemon.rs `run_or_log`):** all via
  `spawn_recorded` (recording); the `staggered_initial_delay_ms` ones also get
  §5. db_health-gated light crons also `record_skip(DbDown)`.
- **Manual `trigger_cron`:** the 14-arm match was split into
  `trigger_cron_dispatch`, wrapped by one `CronRunGuard` (`Manual`) recording
  Ok/Failed/Panicked (busy = lock held = no run, so no row).

### 8. MCP surface + retention

Read-only `cron_history` tool (`src/mcp/tools/tool_cron_history.rs`, registered in
`handlers/inventory.rs` + the `dispatch_tool!` params section + `tools/mod.rs`):
returns `{ by_job, recent }` — per-job rollup (last outcome, last success,
computed `next_due = last_ok + interval`, run/ok/fail/skip counts) + recent runs
with intrinsics. The `cron_history_writes_dropped` counter is surfaced in
`index_stats`. Retention: `cron_history_retention_days` (default 30, `0` =
forever) swept by the existing `db-maintenance` light cron via
`delete_cron_runs_older_than`.

## Trust / observability boundary

The writer path is **append-only** (one INSERT per run, plus the
`db-maintenance` retention DELETE). The ledger is observational: it never gates
or transitions any work item. Privacy posture matches `file_chunks` /
`mcp_tool_calls` — purely local, no remote shipping; `error_detail` carries
internal error messages only.

## Alternatives rejected

- **A second `cron_schedule_state` table** for restart-survival — rejected;
  duplicates state and risks drift. The partial index over the append-only ledger
  serves the hot path directly.
- **crossbeam + a dedicated `std::thread` + `rt.block_on`** (the original plan's
  §3 sketch) — rejected in favor of copying the telemetry writer verbatim (tokio
  `mpsc` + `CancellationToken`), which already solves graceful-shutdown flushing.
- **Keeping the cooldown in the in-pool gate** and retrying from there — rejected;
  the pool task can't cleanly reschedule its own recurring closure. Moving the
  decision to the scheduler thread (where the `CronHandle` + `run_once` live) is
  the natural seam.
- **`DbClient` trait method for the writes** — rejected; free `&PgPool` query
  functions (mirroring `quality_report_history`) leave the 85-method
  `MockDbClient` untouched.

## Verification

- **Pure unit tests (no DB):** vocab goldens; `restart_initial_delay_ms`
  (no-history/overdue/future-due/clock-back/interval-0/anti-herd);
  `cooldown_decision` (zero/not-ready/within-window); `CronRunGuard` outcome
  mapping + `From<CronJobOutcome>`.
- **Real-DB integration** (`pgmcp-testing/tests/cron_history_integration.rs`,
  `require_test_db!`): seeds mixed-outcome rows; asserts the tool's rollup +
  recent; exercises `last_successful_completions` and the retention sweep. (Skips
  cleanly where no test DB is configured.)
- **Full gate:** `./scripts/verify.sh`.
