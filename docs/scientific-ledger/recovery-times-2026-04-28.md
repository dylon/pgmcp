# Recovery times — empirical baselines

**Started:** 2026-04-28
**Author:** Claude (auto, via plan in `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`)

## Why this file exists

Modeled after `daas/docs/ops/swap_recovery_times.md`, which records measured
warm/cold model-swap latencies for NIM model deployments. The pgmcp daemon
has no equivalent record, so deployment expectations ("will it be ready in
30 seconds or 30 minutes?") have to be guessed each time. This ledger is
the place to write them down.

## Measurements to capture

Each row is a single measurement. Add new rows over time; do not edit old
rows once filed (they are the historical record).

### Columns

- **date**: YYYY-MM-DD of the measurement
- **scenario**: one of:
  - `cold-daemon-ready`: daemon start → first `phase=Ready` from `/health`
  - `warm-daemon-ready`: daemon restart with index already populated
  - `embed-pool-warmup`: time from `Initializing` → first `embed_query` succeeds
  - `cold-reindex-throughput`: full reindex of N files, files/sec
  - `warm-reindex-throughput`: incremental reindex (only changed files), files/sec
  - `health-probe-latency`: `curl -m 0.5 /health` round-trip p50/p99
  - `tool-call-latency`: median end-to-end for `semantic_search`/`grep`/`orient`
- **workspace_size_files**: file count of the indexed workspaces
- **observed_seconds**: measured wall-clock seconds (or files/sec for throughput rows)
- **hardware_notes**: GPU model, RAM, disk class
- **notes**: anything unusual (cold cache, OOM nearby, hot config swap, etc.)

## Methodology

For latency rows:
```
# Stop daemon, drop caches if measuring cold:
sudo systemctl stop pgmcp.service
sudo sync && echo 3 | sudo tee /proc/sys/vm/drop_caches  # cold only
# Start, time to Ready:
START=$(date +%s.%N)
sudo systemctl start pgmcp.service
until [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3100/health)" = "200" ]; do sleep 0.1; done
END=$(date +%s.%N)
echo "ready_seconds=$(echo "$END - $START" | bc)"
```

For throughput rows: record `files_indexed` from `/api/status` at start
and end of an indexing run; divide delta by elapsed wall time.

For tool-call latency: `time curl -X POST .../mcp -d '{...}'` against a
warm daemon, repeated 100 times, take p50/p99.

## Harness

`scripts/measure_recovery_times.sh` runs all five scenarios end-to-end:

- emits one markdown row per scenario to stdout (drop-in for the
  Observations table below);
- captures raw stdout to a sibling `.raw.log` for reproducibility;
- self-skips rows whose preconditions aren't met on the local host
  (no `sudo -n` → no `cold-daemon-ready` cache flush; no running
  daemon on `:3100` → no `health-probe-latency`; etc.) with a
  diagnostic line on stderr.

Hardware stamp comes from the first H1 of
`/home/dylon/.claude/hardware-specifications.md`.

In-process `tool-call-latency` is measured via a criterion benchmark
at `benches/mcp_tool_latency.rs` (registered in `Cargo.toml`). The
bench self-skips when the in-memory pgmcp-testing server fixture
isn't available, falling back to a dispatcher-overhead lower-bound
sample so the suite still produces a row on every host.

`embed-pool-warmup` is sourced from the structured-log span
`embed_pool_warmup` added in `src/embed/pool.rs::embedding_worker`
— the worker emits `phase="ready"` with the elapsed seconds the
first time `Embedder::new` succeeds. The harness greps the log file
for the most recent entry.

## Observations

| date       | scenario                  | workspace_size_files | observed | hardware_notes | notes |
|------------|---------------------------|----------------------|----------|----------------|-------|
| 2026-05-22 | tool-call-latency-dispatcher-baseline | n/a | 232 ps median | (host varies) | criterion `mcp_tool_dispatch_overhead_skipped_no_fixture`, 20 samples; a lower bound on tool-call overhead. The full in-process call_tool_cli path becomes measurable once `pgmcp-testing::server_with_pool` is reachable from criterion (i.e., a TestDatabase URL is exported). |
| _pending_  | cold-daemon-ready         |                      |          |                | Run `scripts/measure_recovery_times.sh` with `sudo -n` available. |
| _pending_  | warm-daemon-ready         |                      |          |                | Same harness, daemon already started so `drop_caches` is skipped. |
| _pending_  | embed-pool-warmup         |                      |          |                | Grepped from `~/.local/share/pgmcp/pgmcp.log` for `phase="ready"`. |
| _pending_  | cold-reindex-throughput   |                      |          |                | `pgmcp reindex` from a cold cache; harness times wall-clock + reads `indexed_files` row count. |
| _pending_  | warm-reindex-throughput   |                      |          |                | Same harness, pages cached. |
| _pending_  | health-probe-latency      |                      |          |                | curl `/health` x 100 reps; harness reports p50, p99. |
| _pending_  | tool-call-latency (full)  |                      |          |                | Criterion bench once `pgmcp-testing::server_with_pool` is fixtured. |

## Why this matters

- **Health hooks decisions:** `~/.claude/hooks/lib/pgmcp-common.sh::pgmcp_health_ok` polls with a 300 ms timeout. If cold-daemon-ready is consistently >30 s, hooks correctly silent-exit during that window, which is the desired behavior. If it's >5 min, that's a useful number to put in deployment runbooks.
- **MCP timeout sizing:** `src/mcp/server.rs::timeout_wrap` defaults to 30 s per non-reindex tool. If `tool-call-latency` p99 ever exceeds 30 s for tools that aren't reindex, the budget needs revisiting.
- **Operator expectations:** systemd `Type=notify` with `TimeoutStartSec=` should be set to ~2× the observed cold-daemon-ready time. We don't currently know the right value.

## Related

- daas's analogous file: `/home/dylon/Workspace/f1r3fly.io/daas/docs/ops/swap_recovery_times.md` — measures NIM model swaps (warm Llama 70B ≈ 645 s, warm small models ≈ 169 s).
- `src/daemon_state.rs::DaemonLifecycle` — the source of truth for the phase signal `/health` and `/api/status` both report.
- `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md` — Stage 4b of the utilization plan, which calls for this ledger entry.
