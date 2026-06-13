# pgmcp Configuration Reference

All settings exposed via `config.toml` (global) and per-project
`.pgmcp.toml` overrides. See [the README](../README.md) for installation
and [operations.md](operations.md) for daemon lifecycle.


Generate the default configuration:

```bash
pgmcp init
```

This writes `~/.config/pgmcp/config.toml`. You can also specify a custom path:

```bash
pgmcp --config /path/to/config.toml serve
```

Or set the `PGMCP_CONFIG` environment variable.

### Example Configuration

```toml
[workspace]
paths = [
    "/home/user/projects",
    "/home/user/work",
]

[indexer]
debounce_ms = 300
max_file_size_bytes = 1048576
exclude_patterns = ["node_modules", "target", ".git", "__pycache__", "*.lock"]

[[indexer.file_types]]
extension = "rs"
language = "rust"

[[indexer.file_types]]
extension = "py"
language = "python"

# ... 50+ file types configured by default (rust, python, typescript, javascript,
# java, scala, c, cpp, clojure, clojurescript, rholang, metta, coq, tlaplus, lean,
# sage, prolog, shell, jsonl, markdown, toml, json, yaml, and document/formal
# verification languages — see src/config.rs default_file_types())

[database]
host = "localhost"
port = 5432
name = "pgmcp"
user = "pgmcp"
# password via PGMCP_DB_PASSWORD env var or:
# password = "secret"
max_connections = 40
# Per-session server-side timeouts (milliseconds), applied to every pooled
# connection. Long-running cron queries raise their own ceiling via
# `SET LOCAL` inside a transaction.
statement_timeout_ms = 30000            # cancel any single query after this
idle_in_transaction_timeout_ms = 60000  # cancel a transaction idle this long
lock_timeout_ms = 5000                  # cap any single lock-acquisition wait
# PostgreSQL >= 14 only (silently ignored on older servers). While a backend is
# running a long query it otherwise never notices that its client has gone, so a
# daemon that is killed / crashes mid-query leaves an *orphaned backend* holding
# its locks until `statement_timeout` fires (minutes). With this set, the backend
# polls the client socket every interval and self-terminates once the client is
# gone — releasing its locks so a restarted daemon's startup migrations don't
# collide with the dead instance and abort at `lock_timeout`. 0 disables it.
client_connection_check_interval_ms = 10000

[embeddings]
model = "all-MiniLM-L6-v2"
dimensions = 384
chunk_size_lines = 50
chunk_overlap_lines = 10
batch_size = 32
pool_size = 2
use_gpu = false

[vector]
hnsw_m = 24
hnsw_ef_construction = 200
ef_search = 100

[mcp]
transport = "stdio"
host = "127.0.0.1"
port = 3100

[work_pool]
min_threads = 2
max_threads = 0    # 0 = num_cpus
initial_threads = 0  # 0 = min_threads

[metrics]
http_enabled = true
http_port = 9464
http_bind = "127.0.0.1"

[logging]
file = "~/.local/share/pgmcp/pgmcp.log"
level = "info"
rotation = "daily"
max_log_files = 7
# Output format for the daemon log file: "json" (default), "compact", or "pretty".
# Stderr always uses the compact human-readable form regardless.
format = "json"
# Optional separate file capturing only MCP-tool-call events (`invoked` /
# `completed` / `failed` from `instrumented_tool_run`). Filtered to the
# `pgmcp::mcp::tool` tracing target. Uses the same rotation policy as `file`.
# access_log = "~/.local/share/pgmcp/mcp-access.log"
# Per-target log-level overrides composed with the global `level`. RUST_LOG
# (when set) still wins. Example:
#   [logging.targets]
#   "pgmcp::mcp::tool"  = "debug"   # MCP tool entry/exit events
#   "pgmcp::mcp::tools" = "debug"   # per-tool body-start events
#   "sqlx::query"       = "warn"    # quiet sqlx query logs

[cron]
stale_cleanup_interval_secs = 3600
integrity_check_interval_secs = 86400
stats_aggregation_interval_secs = 60
db_maintenance_interval_secs = 604800
git_history_index_interval_secs = 3600
similarity_scan_interval_secs = 21600
similarity_threshold = 0.85
similarity_top_k = 10
topic_scan_interval_secs = 43200
topic_min_cluster_size = 5
# topic_num_clusters = auto  # omit for auto K estimation
topic_fuzziness = 2.0
graph_analysis_interval_secs = 7200
quality_history_interval_secs = 21600    # snapshot quality GPAs (trend/forecast); 0 disables
tool_policy_interval_secs = 21600        # recompute per-client learned tool surface from usage; 0 disables
findings_promotion_interval_secs = 21600 # auto-promote findings → items (opted-in projects); 0 disables

# Proactive digest — OFF by default (local-first). Rides the SessionStart
# `pgmcp context` CLI and the UserPromptSubmit observe `additional_context`.
[digest]
enabled = false              # master switch; nothing is composed/appended unless true
session_start = true         # append to the SessionStart `pgmcp context` output
prompt = true                # append to the UserPromptSubmit observe additional_context
ttl_secs = 1800              # dedup window: suppress an identical digest to a session within this
max_per_session = 10         # lifetime cap on emissions per session (across channels)
max_bytes = 1024             # byte budget for the rendered block (most-severe items survive truncation)
include_trends = true        # include the TREND (GPA slope/forecast) section
webhook_url = ""             # empty = no outbound POST (opt-in)
webhook_min_severity = "high" # min max_severity to POST: info|notice|high|critical
pg_notify = false            # emit pg_notify('pgmcp_digest', …); reserved seam, no consumer built

# MCP-client tracking — which clients (by OS PID + cwd → project) are connected,
# their liveness, and which files they touch. The PID is recovered from the TCP
# peer via /proc; everything else (cwd, liveness, open files) follows from it.
[clients]
enabled = true               # capture connected clients into mcp_clients (PID/cwd/project/liveness)
file_events = true           # accept POST /api/client/file_event (Claude Code PostToolUse hook)
ebpf_enabled = false         # Phase-2B client-agnostic capture via a bpftrace openat/open probe
                             #   filtered to the live client PIDs (source='ebpf'). Needs bpftrace on
                             #   PATH + CAP_BPF+CAP_PERFMON (or root) at runtime; never affects the
                             #   stable build. Off by default.
ebpf_refresh_secs = 15       # how often the eBPF consumer re-reads the live PID set & respawns
ebpf_dedup_secs = 5          # collapse identical (pid, op, path) eBPF events within this window
proc_fd_supplement = false   # also sample /proc/<pid>/fd on each liveness tick (source='proc_fd');
                             #   near-blind to open-close editors, so low-signal — off by default
```

### Configuration Reference

| Section      | Key                               | Default                             | Description                                    |
|--------------|-----------------------------------|-------------------------------------|------------------------------------------------|
| `workspace`  | `paths`                           | `[]`                                | Directories to watch and index                 |
| `indexer`    | `debounce_ms`                     | `300`                               | Debounce window for file events                |
| `indexer`    | `max_file_size_bytes`             | `1048576`                           | Max file size for content storage              |
| `indexer`    | `exclude_patterns`                | `[node_modules, target, .git, ...]` | Glob/substring patterns to skip                |
| `database`   | `host`                            | `localhost`                         | PostgreSQL host                                |
| `database`   | `port`                            | `5432`                              | PostgreSQL port                                |
| `database`   | `name`                            | `pgmcp`                             | Database name                                  |
| `database`   | `user`                            | `pgmcp`                             | Database user                                  |
| `database`   | `password`                        | `None`                              | Database password (prefer `PGMCP_DB_PASSWORD`) |
| `database`   | `max_connections`                 | `40`                                | Connection pool size                           |
| `embeddings` | `model`                           | `all-MiniLM-L6-v2`                  | Sentence-transformer model name                |
| `embeddings` | `dimensions`                      | `384`                               | Embedding vector dimensions                    |
| `embeddings` | `chunk_size_lines`                | `50`                                | Lines per chunk                                |
| `embeddings` | `chunk_overlap_lines`             | `10`                                | Overlapping lines between chunks               |
| `embeddings` | `batch_size`                      | `32`                                | Embedding batch size                           |
| `embeddings` | `pool_size`                       | `2`                                 | Dedicated embedding threads                    |
| `embeddings` | `use_gpu`                         | `false`                             | Enable CUDA execution provider for ort         |
| `vector`     | `hnsw_m`                          | `24`                                | HNSW max bi-directional links per node         |
| `vector`     | `hnsw_ef_construction`            | `200`                               | HNSW candidate list size during construction   |
| `vector`     | `ef_search`                       | `100`                               | HNSW candidate list size during search         |
| `mcp`        | `transport`                       | `stdio`                             | Transport mode                                 |
| `mcp`        | `host`                            | `127.0.0.1`                         | Daemon bind address                            |
| `mcp`        | `port`                            | `3100`                              | Daemon port                                    |
| `work_pool`  | `min_threads`                     | `2`                                 | Minimum active workers                         |
| `work_pool`  | `max_threads`                     | `0` (num_cpus)                      | Maximum workers                                |
| `work_pool`  | `initial_threads`                 | `0` (min_threads)                   | Workers at startup                             |
| `metrics`    | `http_enabled`                    | `true`                              | Enable Prometheus endpoint                     |
| `metrics`    | `http_port`                       | `9464`                              | Metrics port                                   |
| `logging`    | `file`                            | `~/.local/share/pgmcp/pgmcp.log`    | Daemon log file path                           |
| `logging`    | `level`                           | `info`                              | Log level (composes with `RUST_LOG`)           |
| `logging`    | `rotation`                        | `daily`                             | Log rotation period (`daily`/`hourly`/`never`) |
| `logging`    | `max_log_files`                   | `7`                                 | Rotated-file retention                         |
| `logging`    | `format`                          | `json`                              | File output: `json` / `compact` / `pretty`     |
| `logging`    | `access_log`                      | `None`                              | Optional separate MCP-tool-call access log     |
| `logging.targets` | `<target> = <level>`         | `{}`                                | Per-target level overrides (e.g. `pgmcp::mcp::tool = debug`) |
| `cron`       | `stale_cleanup_interval_secs`     | `3600`                              | Stale file cleanup interval                    |
| `cron`       | `integrity_check_interval_secs`   | `86400`                             | Integrity check interval                       |
| `cron`       | `stats_aggregation_interval_secs` | `60`                                | Stats refresh interval                         |
| `cron`       | `db_maintenance_interval_secs`    | `604800`                            | VACUUM ANALYZE interval                        |
| `cron`       | `git_history_index_interval_secs` | `3600`                              | Git history indexing interval                  |
| `cron`       | `similarity_scan_interval_secs`   | `21600`                             | Cross-project similarity scan interval         |
| `cron`       | `similarity_threshold`            | `0.85`                              | Minimum cosine similarity for pair storage     |
| `cron`       | `similarity_top_k`                | `10`                                | Neighbors per chunk in similarity scan         |
| `cron`       | `topic_scan_interval_secs`        | `43200`                             | Topic clustering interval                      |
| `cron`       | `topic_min_cluster_size`          | `5`                                 | Minimum chunks per topic                       |
| `cron`       | `topic_fuzziness`                 | `2.0`                               | FCM fuzziness exponent                         |
| `cron`       | `graph_analysis_interval_secs`    | `7200`                              | Graph analysis interval                        |
| `cron`       | `quality_history_interval_secs`   | `21600`                             | Quality-GPA snapshot interval (trend/forecast); 0 disables |
| `cron`       | `tool_policy_interval_secs`       | `21600`                             | Adaptive tool-surface refresh interval (recompute per-client defaults via recency-decayed usage frequency); 0 disables |
| `cron`       | `findings_promotion_interval_secs`| `21600`                             | Findings→work-item promotion interval (opted-in projects only); 0 disables globally |
| `digest`     | `enabled`                         | `false`                             | Master switch for the proactive digest         |
| `digest`     | `session_start`                   | `true`                              | Append digest to the SessionStart `pgmcp context` output |
| `digest`     | `prompt`                          | `true`                              | Append digest to the UserPromptSubmit observe `additional_context` |
| `digest`     | `ttl_secs`                        | `1800`                              | Dedup window for an identical digest per session |
| `digest`     | `max_per_session`                 | `10`                                | Lifetime cap on digest emissions per session   |
| `digest`     | `max_bytes`                       | `1024`                              | Byte budget for the rendered digest block      |
| `digest`     | `include_trends`                  | `true`                              | Include the TREND (GPA slope/forecast) section |
| `digest`     | `webhook_url`                     | `""`                                | Optional outbound webhook (empty = off)        |
| `digest`     | `webhook_min_severity`            | `high`                              | Min `max_severity` to POST (`info`/`notice`/`high`/`critical`) |
| `digest`     | `pg_notify`                       | `false`                             | Emit `pg_notify('pgmcp_digest', …)` (reserved seam; no consumer) |
| `clients`    | `enabled`                         | `true`                              | Capture connected MCP clients (PID/cwd→project/liveness) into `mcp_clients` |
| `clients`    | `file_events`                     | `true`                              | Accept `POST /api/client/file_event` (Claude Code PostToolUse hook → `client_file_events`) |
| `clients`    | `ebpf_enabled`                    | `false`                             | Phase-2B client-agnostic capture via a `bpftrace` openat/open probe (needs `CAP_BPF`+`CAP_PERFMON`; off by default) |
| `clients`    | `ebpf_refresh_secs`               | `15`                                | eBPF consumer: live-PID-set re-read & probe-respawn interval |
| `clients`    | `ebpf_dedup_secs`                 | `5`                                 | eBPF consumer: collapse identical `(pid, op, path)` within this window |
| `clients`    | `proc_fd_supplement`              | `false`                             | Sample `/proc/<pid>/fd` on each liveness tick (`proc_fd` source; low-signal) |

### Per-Project Configuration (`.pgmcp.toml`)

Each project can have a `.pgmcp.toml` file in its root directory to override
global settings and enable project-specific features.

**Supported sections:**

- **`[indexer]`** -- override `exclude_patterns`, `file_types`, and
  `max_file_size_bytes` for this project only
- **`[git]`** -- enable git history indexing (`index_history`) and commit→work-item
  auto-linkage (`auto_link_items`) for this project
- **`[tracker]`** -- opt this project into the `findings-promotion` cron
  (`auto_promote_findings`, default **OFF**) and tune its bug-score threshold

**Example `.pgmcp.toml`:**

```toml
[indexer]
exclude_patterns = ["vendor", "dist"]
max_file_size_bytes = 2097152

[git]
index_history = true
# Auto-link commits whose message references a work item (`#<public_id>` or
# `fixes|closes|resolves|implements|refs <public_id>`) and run the agent-grade
# auto-transition (at most → a verify *candidate*, NEVER → verified). Omit for
# the default: ON when `index_history` is on. Set `false` to index history
# without touching the tracker.
auto_link_items = true

[tracker]
# Opt this project into the `findings-promotion` cron: idempotently materialize
# high-confidence `bug_prediction` files (→ `pending` `bug`) and high-severity
# `documented_tech_debt` markers (→ `pending` `fixme`) into the tracker. Default
# OFF — promotion is a write-side action a project opts into explicitly. Promoted
# items land in `pending`, never pre-`confirmed` (confirmation is user-only).
auto_promote_findings = false
# Minimum `bug_prediction` score for a file to be promoted (only consulted when
# `auto_promote_findings` is on).
findings_bug_score_threshold = 0.6
```

**Per-project keys:**

| Section     | Key                            | Default                  | Description                                                                 |
|·············|································|··························|·····························································................|
| `git`       | `index_history`                | `false`                  | Index commit messages + diffs for this project                              |
| `git`       | `auto_link_items`              | (= `index_history`)      | Auto-link + agent-grade auto-transition referenced work items; explicit value wins |
| `tracker`   | `auto_promote_findings`        | `false`                  | Opt into the `findings-promotion` cron for this project                     |
| `tracker`   | `findings_bug_score_threshold` | `0.6`                    | Min `bug_prediction` score to promote a file (when promotion is on)         |

**Managing `.pgmcp.toml`:**

```bash
pgmcp init-project              # Create .pgmcp.toml in $PWD
pgmcp init-project --cwd DIR    # Create .pgmcp.toml in DIR
pgmcp upgrade-project           # Merge new defaults into existing .pgmcp.toml
pgmcp upgrade-project --cwd DIR # Merge new defaults in DIR
```

### Proactive digest (`[digest]`)

The digest turns pgmcp's *pull* surface into *push*: it composes a compact,
severity-sorted block from live **TRACKER** (overdue / blocked / needs-triage /
next-actionable), **HEALTH** (index staleness, embedding backlog, recently-panicked
crons), and **TREND** (Phase-1 GPA slope + forecast) signals, and appends it to
the two channels agents already read — the **SessionStart** `pgmcp context` CLI
output (`session_start`) and the **UserPromptSubmit** `/api/session/observe`
`additional_context` (`prompt`). It is **structurally read-only**: only `SELECT`s
plus one INSERT into its own `digest_emissions` rate-limit ledger (which stores a
content fingerprint + item count, never the digest body or prompt text).

**Local-first defaults:** the whole section is `enabled = false`, so a stock
install never composes or appends a digest. `webhook_url` is empty (no outbound
POST), and `pg_notify` is `false` (the `pg_notify('pgmcp_digest', …)` seam is
wired but has no consumer in the single-user setup). To turn it on:

```toml
[digest]
enabled = true     # the only switch needed for the in-session channels
# session_start / prompt default true; max_per_session=10, ttl_secs=1800 dedup.
# Opt into the outbound webhook by setting a URL (gated by webhook_min_severity):
# webhook_url = "https://hooks.example.com/pgmcp"
```

The TREND section additionally depends on the **`quality-history`** cron having
populated `quality_report_history` (so set `[cron] quality_history_interval_secs`
> 0, the default); with no history the digest simply carries TRACKER + HEALTH.

### Environment Variables

| Variable            | Description                                                                                                                                  |
|---------------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| `PGMCP_DB_PASSWORD` | Database password (takes precedence over config file)                                                                                        |
| `PGMCP_CONFIG`      | Path to configuration file                                                                                                                   |
| `RUST_LOG`          | Tracing filter (default `info`). Applies to all CLI subcommands and `serve`/`daemon`. Output goes to stderr; stdout stays clean for piping.  |

#### Diagnosing CLI failures

Every CLI subcommand (`analyze`, `reindex`, `tool`, `context`, `statistics`,
`status`, `results`) installs a tracing subscriber that writes to stderr,
respecting `RUST_LOG`. If a CLI run finishes with surprising output (zero
results, empty tables), re-run with `RUST_LOG=info` (or `debug`) to see the
internal log stream. Long-running analyses (topic clustering on a large
corpus, full reindex) emit progress + error messages along the way.

```bash
RUST_LOG=info pgmcp analyze topics 2>&1 | tee /tmp/topics.log
```

---

