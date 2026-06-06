# Trigger Cron Serialization Formal Verification Traceability

Status: focused high-use operational slice for `trigger_cron`.

## Scope

The current telemetry ordering showed `trigger_cron` as both high-use and
error-prone. It is also a concurrency-sensitive tool because it manually runs
maintenance jobs that are normally serialized by the scheduler's heavy-cron
lock.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `trigger_cron` | Normalize the job name and optional project; reject blank or unknown jobs before acquiring the heavy-cron lock; share one heavy-cron lock with scheduled jobs; acquire that lock non-blocking; return `status="busy"` instead of queueing when the lock is held; never start a cron body on `busy` or invalid dispatch; release the lock and clear `heavy_cron_running` on every completed/error return. | `tla/TriggerCronSerialization.tla`; `pgmcp-testing/tests/tool_trigger_cron.rs`; `pgmcp-testing/tests/trigger_cron_project_param.rs`. |

## Issues Found And Corrected

`tool_trigger_cron.rs` documented non-blocking heavy-cron serialization but did
not acquire the scheduler's heavy-cron lock. Manual MCP calls could therefore
run concurrently with scheduled heavy crons or with another manual trigger.

Correction: `SystemContext` now owns a shared `heavy_cron_lock`, scheduled crons
use that same lock, and `trigger_cron` uses `try_lock` on it before dispatching
any cron body.

`trigger_cron` also matched the raw job string. Correction: the job name is now
trimmed, blank jobs are rejected, and the accepted set remains explicit:
`symbol-extraction | call-graph | function-metrics | graph-analysis |
a2a-reflect | msm-calibrate | fuzzy-sync`.

The optional `project` field is now trimmed and blank values are treated as
absent. Project values are only forwarded to the three per-project cron entry
points (`symbol-extraction`, `call-graph`, and `function-metrics`).

## Concurrency Boundary

The manual trigger and the scheduler now share one lock domain. The lock is
non-blocking: a held lock produces a structured JSON `busy` response with
`retry_after_secs=60`, and no cron body is queued. This avoids deadlock between
operator-triggered jobs and scheduled heavy jobs, prevents duplicate heavy jobs
from running concurrently, and keeps the MCP request path from waiting behind a
long GPU or database maintenance job.

The lock is a `tokio::sync::Mutex`, so the MCP async path does not hold a
blocking mutex guard across `.await`. Scheduled crons still acquire it with
`try_lock` from worker threads.

## Formal Model

`tla/TriggerCronSerialization.tla` models valid, unknown, and blank job names;
whitespace normalization; blank and nonblank projects; free and held initial
lock states; completed, busy, and rejected responses; and project forwarding
for scoped cron jobs.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidJobsRejectedBeforeLock` | Blank/unknown jobs do not acquire the lock or start a body. |
| `BusyNeverRunsBody` | Valid calls that observe a held lock return `busy`, do not queue, and do not start a body. |
| `CompletedOnlyFromFreeLock` | A cron body starts only after acquiring a free lock. |
| `LockReleasedAfterCompletion` | Completed calls release the lock and clear the heavy-cron flag. |
| `NoQueueing` | Manual trigger never queues heavy work. |
| `NormalizedAcceptedJob` | Busy/completed responses expose a normalized valid job name. |
| `ProjectNormalized` | Optional project values are trimmed, with blanks represented as absent. |
| `ProjectForwardingOnlyForScopedJobs` | Project-scoped forwarding is limited to the three supported per-project cron bodies. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=768M \
      PGMCP_TLC_METASPACE=32m PGMCP_TLC_CLASS_SPACE=16m \
      PGMCP_TLC_CODE_CACHE=32m \
      ../../../scripts/tlc-capped.sh TriggerCronSerialization.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 10 distinct states, 20
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test tool_trigger_cron \
  --test trigger_cron_project_param --build-jobs 1
```

Result: 4/4 passed. The focused run covers blank/default project parsing,
unknown-job rejection, whitespace-trimmed valid job names, and the busy response
when the shared heavy-cron lock is already held.

```bash
cargo nextest run -p pgmcp heavy_gate --build-jobs 1
```

Result: 6/6 passed across the library and binary test targets. The run covers
the scheduler gate's phase, shutdown, and pass-through behavior after the
shared lock type change.
