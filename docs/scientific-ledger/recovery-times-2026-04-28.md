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

## Observations

| date       | scenario                  | workspace_size_files | observed | hardware_notes | notes |
|------------|---------------------------|----------------------|----------|----------------|-------|
| _pending_  | cold-daemon-ready         |                      |          |                | First baseline goes here once Stage 1 hooks are deployed and we want to know what 503 vs 200 looks like in practice. |

## Why this matters

- **Health hooks decisions:** `~/.claude/hooks/lib/pgmcp-common.sh::pgmcp_health_ok` polls with a 300 ms timeout. If cold-daemon-ready is consistently >30 s, hooks correctly silent-exit during that window, which is the desired behavior. If it's >5 min, that's a useful number to put in deployment runbooks.
- **MCP timeout sizing:** `src/mcp/server.rs::timeout_wrap` defaults to 30 s per non-reindex tool. If `tool-call-latency` p99 ever exceeds 30 s for tools that aren't reindex, the budget needs revisiting.
- **Operator expectations:** systemd `Type=notify` with `TimeoutStartSec=` should be set to ~2× the observed cold-daemon-ready time. We don't currently know the right value.

## Related

- daas's analogous file: `/home/dylon/Workspace/f1r3fly.io/daas/docs/ops/swap_recovery_times.md` — measures NIM model swaps (warm Llama 70B ≈ 645 s, warm small models ≈ 169 s).
- `src/daemon_state.rs::DaemonLifecycle` — the source of truth for the phase signal `/health` and `/api/status` both report.
- `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md` — Stage 4b of the utilization plan, which calls for this ledger entry.
