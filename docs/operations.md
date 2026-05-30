# pgmcp Operations Guide

Daemon lifecycle, cron jobs, and the systemd integration. For monitoring
endpoints see [monitoring.md](monitoring.md); for REST/MCP API surface see
[rest-api.md](rest-api.md) / [mcp-capabilities.md](mcp-capabilities.md).


### Daemon Phases

The daemon transitions through monotonically increasing phases:

| Phase            | Description                                     |
|------------------|-------------------------------------------------|
| **Initializing** | Database, embedding model, thread pools created |
| **Scanning**     | Initial file scan and embedding in progress     |
| **Ready**        | Initial scan complete; all systems nominal      |
| **Terminating**  | Orderly shutdown in progress                    |
| **Defunct**      | Unrecoverable error                             |

Heavy analysis jobs gate on **Ready** to avoid competing with the initial scan for
resources. Light maintenance jobs check only `is_stopping()`.

### Cold-start readiness (liveness ≪ serving-ready ≪ fully-indexed)

Three distinct startup milestones, reached in order — the first two within
seconds of boot:

| Milestone         | Means                                                     | Observable via                                                     |
|-------------------|-----------------------------------------------------------|--------------------------------------------------------------------|
| **Listening**     | HTTP listener bound; process is up                        | TCP connect succeeds; `recovery_times phase="listening"`           |
| **Serving-ready** | DB migrated + ≥1 embedder worker loaded → search/RAG work | `GET /health` → 200; `recovery_times phase="ready"` (embed warmup) |
| **Fully-indexed** | Initial file scan complete (phase → `Ready`)              | `recovery_times phase="scan_complete"`; heavy crons start          |

The listener binds **right after migrations**, *before* the optional reranker /
LLM-extractor model loads (which now load in the background and hot-swap in) and
without waiting for the embedder workers (each loads its model in its own
thread). So `/health` is reachable almost immediately and flips to 200 as soon as
one embedder worker is up — even while the initial scan is still running.
`/api/search` and the `semantic_search` / `hybrid_search` MCP tools return a fast
503 / retryable error until an embedder worker is ready, rather than blocking.

**Cold-start scan throughput (data-driven).** The initial scan runs on a
background thread and does *not* gate listening or serving-readiness. The
directory walk is parallelized (the `ignore` crate's `build_parallel`), but the
walk is not the bottleneck: on a warm DB it is mostly metadata-skip `stat`s
(~seconds for 20k+ files), and on a cold DB the cost is dominated by **embedding
throughput** — already parallel across `embeddings.pool_size` workers and bounded
by GPU VRAM. To make a cold full-index finish sooner, raise
`embeddings.pool_size` / `embeddings.batch_size` (VRAM permitting), not the walk.

### Logs

The daemon writes structured JSON logs to the file named by `[logging] file`
(default `~/.local/share/pgmcp/pgmcp.log`). The level is `[logging] level`
(default `info`); `RUST_LOG` overrides it. One-shot CLI subcommands log to stderr
*and* append to the same file.

**Rotation.** `[logging] rotation` (default `daily`) rotates by **renaming** the
active file to `pgmcp.log.<period>` — `<period>` is a **UTC** calendar date
(`2026-05-30`) for `daily`, or `…-HH` for `hourly` — then opens a fresh
`pgmcp.log`. `[logging] max_log_files` (default 7) bounds how many rotated files
are retained. Rotation is keyed to **UTC** so the rotated-file date suffix lines
up with the UTC timestamps inside the log.

**Watch the live log with `tail -F`, not `tail -f`.** Rotation renames the inode,
and `tail -f` follows the open *descriptor* — so it silently freezes on the
now-rotated file the instant the log rolls over (UTC midnight for `daily`); the
log then *looks* stuck even though the daemon is happily writing to the new
`pgmcp.log` inode. `-F` follows the *path* and re-opens across the rename:

```sh
tail -F ~/.local/share/pgmcp/pgmcp.log          # survives rotation
tail -F ~/.local/share/pgmcp/pgmcp.log | jq .   # pretty-print the JSON lines
```

A long, fully-blocking startup **migration** (e.g. a `GENERATED … STORED` column
add that rewrites a large table) can take minutes during which the daemon emits
nothing else and the HTTP listener is not yet bound — the migration runner now
logs a `"starting migration step"` line (with the version) before each step and a
`"migration step applied"` line with `elapsed_ms` after, so a long step reads as
*in progress* rather than as a hang.

### Cron Jobs

pgmcp runs eight automated jobs via a lock-free cron state machine:

| Job                 | Default Interval | Gate  | Description                                                                       |
|---------------------|------------------|-------|-----------------------------------------------------------------------------------|
| `stats-aggregation` | 60 s             | Light | Refresh in-memory statistics counters                                             |
| `stale-cleanup`     | 1 h              | Light | Remove files from the index that no longer exist on disk                          |
| `git-history-index` | 1 h              | Ready | Incrementally index git commit history for opted-in projects                      |
| `integrity-check`   | 24 h             | Light | Delete files with `NULL` content_hash (incomplete indexing)                       |
| `graph-analysis`    | 2 h              | Ready | Extract imports (8 languages), build graph, compute PageRank/betweenness/coupling |
| `similarity-scan`   | 6 h              | Ready | Cross-project chunk similarity via HNSW batch scan                                |
| `topic-clustering`  | 12 h             | Ready | Fuzzy C-Means + c-TF-IDF topic discovery across all projects                      |
| `db-maintenance`    | 7 d              | Light | `VACUUM ANALYZE` on core tables                                                   |

All intervals are configurable in the `[cron]` section of the config file.

---

