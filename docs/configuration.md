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

### Per-Project Configuration (`.pgmcp.toml`)

Each project can have a `.pgmcp.toml` file in its root directory to override
global settings and enable project-specific features.

**Supported sections:**

- **`[indexer]`** -- override `exclude_patterns`, `file_types`, and
  `max_file_size_bytes` for this project only
- **`[git]`** -- enable git history indexing for this project

**Example `.pgmcp.toml`:**

```toml
[indexer]
exclude_patterns = ["vendor", "dist"]
max_file_size_bytes = 2097152

[git]
index_history = true
```

**Managing `.pgmcp.toml`:**

```bash
pgmcp init-project              # Create .pgmcp.toml in $PWD
pgmcp init-project --cwd DIR    # Create .pgmcp.toml in DIR
pgmcp upgrade-project           # Merge new defaults into existing .pgmcp.toml
pgmcp upgrade-project --cwd DIR # Merge new defaults in DIR
```

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

