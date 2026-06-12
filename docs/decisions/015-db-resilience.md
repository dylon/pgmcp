# ADR-015 — Database-outage resilience: circuit breaker, disk watchdog, ephemeral-event outbox

- **Status:** Accepted, implemented 2026-06-12
- **Scope:** `src/health/` (new subsystem), `src/stats/tracker.rs` (threading seam),
  `src/cron/scheduler.rs`, `src/embed/pool.rs`, `src/api/handlers.rs`, `src/config.rs`,
  `src/cli/daemon.rs`
- **Supersedes / relates to:** none (new subsystem)

## Context — the 2026-06-11 incident

The host's `/` partition (`/dev/nvme0n1p4`, 2.2 TB) filled to 100%. PostgreSQL's
checkpointer hit `No space left on device` writing
`pg_logical/replorigin_checkpoint.tmp` and **PANIC'd** (SIGABRT, core dumped).
The postmaster then attempted automatic crash recovery, but WAL redo **also**
failed on ENOSPC trying to extend `pg_multixact/members/0001` —
so recovery aborted (`startup process exited with exit code 1`) and, because the
systemd unit is `Restart=no`, Postgres **stayed down for ~1 h 54 m**.

```
20:16:36 EDT  PANIC: could not write to file "pg_logical/replorigin_checkpoint.tmp": No space left on device
20:16:37 EDT  database system was not properly shut down; automatic recovery in progress
              FATAL: could not access status of transaction 0
              DETAIL: Could not write to file "pg_multixact/members/0001" at offset 81920: No space left on device.
              startup process (PID 2181277) exited with exit code 1 → shutting down due to startup process failure
```

During the outage pgmcp had **no shared notion that the DB was down**, so every
DB-touching operation independently:

1. paid the full 10 s `acquire_timeout` against a dead pool, then
2. logged a WARN/ERROR — **1447 `PoolTimedOut` log lines** for one outage, across
   the embed workers and *every* cron (the first pgmcp timeout was 00:16:48 UTC,
   12 s after the PANIC — exact correlation). When Postgres was restarted, pgmcp
   **auto-reconnected cleanly** (sqlx pool churn) with no pgmcp restart needed.

### Root cause and a rejected hypothesis

Root cause: **host disk exhaustion**, external to pgmcp. "No Postgres connections
open" was fully explained — the server itself was dead. A **connection-leak
hypothesis was investigated and rejected**: GPU inference happens *before* any
`pool.begin()`, sqlx transactions auto-rollback on drop, and the 1447 timeouts
span exactly the outage window with clean auto-recovery. There is no leak.

pgmcp cannot stop Postgres from dying on ENOSPC (that is a host-runbook concern,
see the appendix), but it can (1) survive an outage quietly, (2) stop
*contributing* to disk fills, and (3) not silently lose the few fire-and-forget
external writes that have no other durable source.

## Decision

Three cooperating parts under a new `src/health/` module, all reading one shared
state bundle.

### 1. DB-availability circuit breaker (`db_health.rs` + `prober.rs`)

`DbHealth` is a lock-free atomic (`up` / `down_since_epoch` / `generation`).
A **single** background prober (`spawn_db_prober`) runs `SELECT 1` every
`[database] health_probe_interval_secs` (default 10), each probe bounded by
`health_probe_timeout_secs` (default 5, < the 10 s `acquire_timeout` so a hung
pool can't stall the prober a full cycle). The prober is the only writer and the
only logger of DB state, logging **exactly one line per edge** (Up→Down,
Down→Up). Consumers consult `is_up()` and short-circuit:

- **Heavy crons** — a new `SkipReason::DbDown` gate in the `heavy_gate_or_skip!`
  macro (`scheduler.rs`).
- **Light crons** (`work-item-presence`, `mcp-client-liveness`, `git-state-scan`)
  — a `Skipped(DbDown)` record replaces their per-tick `warn!`.
- **Embed pool** — an **intake gate** in `run_worker_event_loop`: when down, the
  worker does not pull from `index_rx`, so unconsumed files stay buffered on the
  bounded channel and the scanner backpressures (no indexing work is
  pulled-and-dropped). Queries (read-only) keep flowing.
- **`/health`** — now reads the breaker (`db_ready = breaker.up && pool.is_some()`)
  instead of the cached `pool().is_some()`, which stayed `true` for the whole
  outage. A pure atomic read; still no DB query on the hot path. Reports
  `db_down_since` when down.

Net: the 1447-line flood collapses to **two lines per outage**; live state is
visible in `/api/status` (`db_up` / `db_down_since` / `disk_pressure` / …).

**"No lost work" decision (embed pool):** if the DB drops *mid-task* (a worker
already holds a file), the existing bounded retry helpers run; on ultimate
failure the file is re-picked-up by the mtime `rescan_workspace` reconciliation
(`indexer/event_processor.rs`). We deliberately do **not** re-queue in-worker:
the worker holds no `index_tx`, and a self-enqueue onto the bounded channel it is
the sole consumer of can deadlock when full. The intake gate covers the common
case (down *before* the pull) losing nothing; rescan covers the rare mid-task case.

### 2. Disk-space watchdog (`disk_pressure.rs` + `watchdog.rs` + `fs.rs`)

Complements — does not duplicate — the `target-cleanup` cron (which reclaims disk
on a 7-day interval). `spawn_disk_watchdog` polls free **bytes and inodes** (a
disk can ENOSPC on either; `fs::fs_avail` reads `statvfs` `f_bavail`/`f_favail`)
across the watched filesystems, taking the worst case. A pure `decide()` applies
hysteresis: enter pressure if *either* axis is below its pause floor; exit only
when *both* are back above their (strictly-greater) resume floors. On the enter
edge it sets `DiskPressure` (pausing the embed intake gate + heavy crons via
`SkipReason::DiskPressure`) and triggers `run_target_cleanup(...)` out-of-band —
reusing all of the cron's safety machinery (dry-run default, `safe_remove`
chokepoint, self-project allowlist). `[disk_guard] pause_floor_gb = 0` disables it.

### 3. Ephemeral-event outbox (`outbox.rs`)

Store-and-forward for the only writes with **no other durable source**: the
fire-and-forget hook ingress `POST /api/session/observe` and
`POST /api/client/file_event`. **File-indexing is deliberately NOT spooled** —
the files on disk plus `rescan_workspace` are a strictly better durable log
(survive reboot; idempotent; no GPU-recompute spool that risks a self-inflicted
ENOSPC).

Mechanism: a **generic deferred local POST**. When the breaker reports down, the
handler spools the *raw request body + endpoint path* (`OutboxRecord`, JSONL) and
returns a neutral response; on the Down→Up edge the prober fires
`OutboxReplayer::replay`, which re-POSTs each record to the *same* loopback
handler. This needs **zero refactoring** of the hot handlers (which resolve
project/file and embed at request time — none of which is possible while down)
and cannot drift from the live path. Idempotency is inherited from the handlers
(`session_prompts` sha256-dedup; `client_file_events` consumed via `DISTINCT ON
(abs_path) … ORDER BY ts DESC`).

**ENOSPC self-defeat guards** (the outage was disk-full, so a naive spool on the
same FS would fail identically and an unbounded spool would *become* the next
ENOSPC): `append` refuses when its own filesystem is below `self_floor` (bytes or
inodes) and caps the spool at `max_bytes` (`stop` or `drop_oldest`); both drops
are counted. The spool dir defaults to `$XDG_STATE_HOME/pgmcp/outbox` with a
strong recommendation to point `[outbox] dir` at a separate volume or tmpfs.

## The threading seam — why `StatsTracker`

`DbHealth` and `DiskPressure` hang off `Arc<StatsTracker>`, which is **already**
`Arc`-threaded into the embed pool, the cron scheduler, every light-cron closure,
and the REST `/health`/`/api/status` handlers. This needs **zero new constructor
parameters**. The alternative (new `SystemContext` fields + new params on
`EmbeddingPool::new`, `spawn_cron`, `schedule_maintenance_jobs`, and the
light-cron `run_or_log`s) would touch ~10 signatures, and the embed pool does not
even receive a `SystemContext`. Verified against the live signatures before
choosing.

## Configuration

```toml
[database]
health_probe_interval_secs = 10   # prober cadence
health_probe_timeout_secs  = 5    # per-probe bound (< 10s acquire_timeout)

[disk_guard]                      # 0 pause_floor_gb disables the whole guard
poll_interval_secs   = 30
warn_floor_gb        = 20
pause_floor_gb       = 10
resume_floor_gb      = 25         # clamped > pause at runtime
warn_floor_inodes    = 2000000
pause_floor_inodes   = 1000000
resume_floor_inodes  = 3000000    # clamped > pause at runtime
# paths = []  → falls back to [cron.target_cleanup] roots, then [workspace] paths, then "/"

[outbox]
enabled            = true
# dir = ""        → $XDG_STATE_HOME/pgmcp/outbox; RECOMMEND a separate FS / tmpfs
max_bytes          = 268435456    # 256 MiB
self_floor_gb      = 2
self_floor_inodes  = 100000
on_full            = "stop"       # or "drop_oldest"
```

Defaults make the breaker + watchdog + outbox **on out of the box**, self-limiting.

## Consequences

- A multi-hour DB outage now produces ~2 log lines instead of ~1447; crons report
  `skipped:db_down` and the embed pool idles instead of churning failed acquires.
- `/health` is now truthful during an outage (503 + `db_down_since`), so the
  UserPromptSubmit hook's fallback works.
- pgmcp pauses its own indexing/heavy-cron disk growth under pressure and kicks
  the existing cleaner, reducing its contribution to a fill.
- Ephemeral hook events survive an outage (bounded, idempotent replay) unless the
  outbox's own filesystem is also exhausted.
- Activates on the next daemon restart.

## Appendix — host-level follow-ups (out of pgmcp scope)

These are the actual root-cause fixes; pgmcp can only make itself *survive*:

1. **systemd `Restart=on-failure`** on `postgresql.service` (currently
   `Restart=no`), so a checkpointer PANIC auto-recovers rather than staying down.
2. **Filesystem headroom on `/`** (reserved blocks / a dedicated PG data volume /
   an OS disk-pressure alert) so crash-recovery can't itself fail on ENOSPC.
3. **Immediate reclamation:** activate the `target-cleanup` cron
   (`[cron.target_cleanup] dry_run = false` after reviewing a dry-run manifest),
   clear the ~4 GB of systemd coredumps, cap the journal.

## Tests

- `db_health.rs`: edge state machine + a concurrency test (exactly one Up→Down
  edge under contention).
- `disk_pressure.rs`: enter/exit edges.
- `watchdog.rs`: table-driven `decide()` — above/warn/enter on **bytes and
  inodes**, the paused dead-band, resume-requires-both-axes, disabled-axis.
- `outbox.rs`: append/segment round-trip, self-floor drop, cap (stop), segment
  finish delete-vs-remainder.
- `stats/tracker.rs`: a seam test driving `DbHealth`/`DiskPressure` through the
  real `StatsTracker` and asserting `/api/status` reflects the state.
