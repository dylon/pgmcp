# **pgmcp**

### A high-performance PostgreSQL + pgvector MCP file indexer for Claude Code

pgmcp continuously indexes your source code into PostgreSQL with vector
embeddings, full-text search indices, and content hashing -- then exposes it all
through the [Model Context Protocol](https://modelcontextprotocol.io/) so Claude
Code can search, read, and reason over your entire codebase at machine speed.

Think of it as a **persistent, queryable mirror** of your file system: every save
triggers an incremental re-index through a lock-free reactive pipeline, and every
MCP tool call retrieves results backed by cosine similarity over HNSW indices,
PostgreSQL's battle-tested GIN/tsvector full-text search, or server-side regex.
The adaptive thread pool scales itself in real time using a hill-climbing
optimizer, so pgmcp stays responsive whether you have 50 files or 500,000.

---

## Features

- **Semantic search** -- vector embeddings via [fastembed](https://github.com/Anush008/fastembed-rs) (all-MiniLM-L6-v2, 384 dimensions)
- **Full-text search** -- PostgreSQL `tsvector`/`tsquery` with GIN index and TF-IDF ranking
- **Regex grep** -- server-side `~` operator across indexed file contents
- **Real-time file watching** -- `notify` crate with debounced event processing
- **Adaptive thread pool** -- hill-climbing autoscaler with exponential moving averages
- **Lock-free reactive pipeline** -- crossbeam channels for zero-mutex data flow
- **Two-phase commit** -- atomic indexing with content hash finalization
- **Incremental indexing** -- xxHash3 content hashing skips unchanged files
- **Streamable HTTP transport** -- multi-client daemon mode for shared team indexing
- **Prometheus metrics** -- `/metrics` endpoint + `pgmcp stats` CLI
- **systemd integration** -- `sd-notify` ready/stopping protocol
- **15 file types** -- Rust, Python, TypeScript, JavaScript, Rholang, MeTTa, Prolog, Markdown, TOML, JSON, YAML, and more
- **Per-project overrides** -- `.pgmcp.toml` in project roots for custom exclusions and file types
- **CUDA acceleration** -- optional GPU-accelerated embeddings via ONNX Runtime
- **Auto-RAG context injection** -- `SessionStart` hook injects project context; `UserPromptSubmit` hook injects semantically relevant code snippets on every prompt
- **REST API** -- `POST /api/search` and `GET /api/context` endpoints alongside the MCP server (daemon mode)

---

## Architecture Overview

```
                          ┌─────────────────────────────────────────────────────────┐
                          │                     pgmcp daemon                        │
                          │                                                         │
  ┌──────────┐   notify   │  ┌─────────┐  filter ┌──────────┐  debounce             │
  │  File    ├───events──▶│  │ Watcher ├────────▶│  Event   ├──────────┐            │
  │  System  │            │  └─────────┘         │  Filter  │          │            │
  └──────────┘            │                      └──────────┘          │            │
                          │                                            ▼            │
                          │  ┌───────────────────────────────────────────────┐      │
                          │  │            WorkPool (adaptive)                │      │
                          │  │                                               │      │
  ┌──────────┐   scan     │  │  ┌─────────┐                                  │      │
  │ Scanner  ├──(LOW)────▶│  │  │ Worker  │◀── HIGH priority (live events)   │      │
  │ (bulk)   │            │  │  │  0..N   │                                  │      │
  └──────────┘            │  │  └────┬────┘◀── LOW priority (scan)           │      │
                          │  │       │                                       │      │
                          │  │       │  ┌──────────────────────┐             │      │
                          │  │       │  │  Scaling Monitor     │             │      │
                          │  │       │  │  J(N) = objective    │             │      │
                          │  │       │  │  ±1 hill climber     │             │      │
                          │  │       │  └──────────────────────┘             │      │
                          │  └───────┊───────────────────────────────────────┘      │
                          │          │ process_file                                 │
                          │          ▼                                              │
                          │  ┌──────────────┐    ┌──────────────────┐               │
                          │  │   Chunker    ├───▶│ Embedding Pool   │               │
                          │  │ (50-line     │    │ (dedicated       │               │
                          │  │  windows)    │    │  threads, each   │               │
                          │  └──────────────┘    │  owns a model)   │               │
                          │                      └────────┬─────────┘               │
                          │                               │                         │
                          │                               ▼                         │
                          │                      ┌──────────────────┐               │
                          │                      │   PostgreSQL     │               │
                          │                      │  + pgvector      │               │
                          │                      │                  │               │
                          │                      │  projects        │               │
                          │                      │  indexed_files   │               │
                          │                      │  file_chunks     │               │
                          │                      └────────┬─────────┘               │
                          │                               │                         │
                          │          ┌────────────────────┘                         │
                          │          ▼                                              │
                          │  ┌──────────────────┐   ┌──────────────────┐            │
                          │  │    MCP Server    │   │    REST API      │            │
                          │  │  (rmcp v1.1)     │   │  /api/search     │            │
                          │  │   at /mcp        │   │  /api/context    │            │
                          │  └───────┬──────────┘   └────────┬─────────┘            │
                          │          │                       │                      │
                          └──────────┊───────────────────────┊──────────────────────┘
                                     │                       │
               ┌─────────────────────┼─────────────────┬─────┘
               │                     │                 │
               ▼                     ▼                 ▼
      ┌────────────────┐   ┌────────────────┐  ┌──────────────────────┐
      │  Claude Code   │   │  Claude Code   │  │  Claude Code Hooks   │
      │  (stdio)       │   │  (HTTP/MCP)    │  │  SessionStart →      │
      └────────────────┘   └────────────────┘  │    pgmcp context CLI │
                                               │  UserPromptSubmit →  │
                                               │    /api/search HTTP  │
                                               └──────────────────────┘
```

**Key data flow:**

1. **File system events** flow through the watcher, get filtered by extension and
   exclusion patterns, then debounced by path (default 300ms)
2. **Debounced events** are dispatched at **HIGH** priority to the adaptive WorkPool
3. **Bulk scan** paths enter at **LOW** priority -- live edits always take precedence
4. **Workers** read the file, compute xxHash3, check the DB for changes, chunk the
   content into overlapping windows, and submit chunks to the embedding pool
5. **Embedding workers** (each owning its own model instance) batch-embed chunks and
   upsert them with their vectors into PostgreSQL
6. **Two-phase commit:** the file's `content_hash` is set to `NULL` during
   processing and only finalized after all chunks succeed -- crash-safe by design
7. **MCP clients** query the index via semantic search, text search, grep, or
   direct file read over stdio or Streamable HTTP

---

## How It Works

### Indexing Pipeline

The indexing pipeline is a reactive, lock-free chain of crossbeam channels:

```
FileEvent(path, kind)
  │
  ├─ Filter: is_configured_extension(path) ∧ ¬excluded(path)
  │
  ├─ Debounce: coalesce events by path within Δt window
  │
  ├─ Dispatch: submit to WorkPool at priority ∈ {HIGH, LOW}
  │
  └─ process_file(path):
       1. content ← read(path)
       2. h ← xxHash3(content)
       3. if DB.content_hash[path] = h then SKIP
       4. file_id ← UPSERT indexed_files (content_hash = NULL)
       5. DELETE old chunks WHERE file_id = file_id
       6. chunks ← chunk(content, size=50, overlap=10)
       7. SEND chunks → EmbeddingPool
       8. EmbeddingPool:
            embeddings ← model.embed(chunks)
            for (chunk, embedding) in zip(chunks, embeddings):
                INSERT file_chunks (chunk, embedding)
            UPDATE indexed_files SET content_hash = h   ← finalize
```

The two-phase commit ensures that a crash mid-indexing leaves `content_hash = NULL`,
which the integrity-check cron job detects and cleans up on the next cycle. No
partial state persists.

### Search Modes

pgmcp provides three complementary search strategies:

**Semantic Search** -- finds conceptually related code even when terminology differs.
The query is embedded into the same 384-dimensional vector space, then ranked by
cosine similarity via pgvector's HNSW index:

```
score(q, c) = 1 − cosine_distance(embed(q), embed(c))
```

**Text Search** -- leverages PostgreSQL's mature full-text search engine. The query
is parsed into a `tsquery`, matched against pre-built `tsvector` GIN indices, and
ranked by TF-IDF:

```
rank = ts_rank(to_tsvector('english', content), plainto_tsquery('english', query))
```

**Grep** -- server-side regex matching via PostgreSQL's `~` operator. Supports
optional glob-based file filtering. Best for precise pattern matching when you know
exactly what you're looking for.

---

## Adaptive Thread Pool

The WorkPool dynamically scales its active worker count using a two-term objective
function minimized by a hill-climbing optimizer.

### Exponential Moving Average (EMA)

Each metric is smoothed with an EMA to filter noise:

```
ēₜ = α · xₜ + (1 − α) · ēₜ₋₁
```

where α = 0.15 (half-life ~ 4.3 samples at 200ms intervals ~ 860ms).

### Objective Function

The scaling monitor minimizes:

```
J(N) = −w_tp · ēma(throughput) + w_qd · ēma(queue_depth)
```

| Weight | Default | Effect                             |
|--------|---------|------------------------------------|
| w_tp   | 1.0     | Reward higher throughput           |
| w_qd   | 2.0     | Penalize queue buildup (2x weight) |

Lower J(N) is better: high throughput with low queue depth.

### Hill Climber

The optimizer uses ±1 perturbation with geometric acceleration:

```
procedure SCALING_MONITOR:
    prev_completed ← pool.tasks_completed()
    loop every 200ms:
        throughput ← pool.tasks_completed() − prev_completed
        queue_depth ← pool.queue_depth()
        tp ← ema_throughput.update(throughput)
        qd ← ema_queue_depth.update(queue_depth)
        J ← −w_tp · tp + w_qd · qd

        improvement ← prev_J − J
        if improvement ≥ threshold:
            step_size ← min(step_size × 2, max_threads / 4)
            apply(direction, step_size)        // unpark or park
            cooldown ← 5 ticks
        elif improvement ≤ −threshold:
            direction ← −direction             // reverse
            step_size ← 1                      // reset acceleration
            apply(direction, 1)
        else:
            HOLD

        prev_J ← J
        prev_completed ← pool.tasks_completed()
```

Step size doubles on consecutive improvements in the same direction (geometric
acceleration), capped at `max_threads / 4`. On reversal, step size resets to 1.
A 5-tick cooldown (1 second) follows each scaling action to let the system
stabilize before measuring again.

---

## Database Schema

```
┌────────────────────┐       ┌────────────────────────┐       ┌──────────────────────┐
│     projects       │       │    indexed_files       │       │    file_chunks       │
├────────────────────┤       ├────────────────────────┤       ├──────────────────────┤
│ id          SERIAL │──┐    │ id         BIGSERIAL   │──┐    │ id        BIGSERIAL  │
│ workspace_path TEXT│  │    │ project_id INTEGER     │  │    │ file_id   BIGINT     │
│ path     TEXT (UQ) │  │    │ path       TEXT (UQ)   │  │    │ chunk_index INTEGER  │
│ name          TEXT │  └───▶│ relative_path TEXT     │  │    │ content   TEXT       │
│ discovered_at  TZ  │       │ language      TEXT     │  └───▶│ start_line INTEGER   │
│ last_scanned_at TZ │       │ size_bytes    BIGINT   │       │ end_line   INTEGER   │
└────────────────────┘       │ content       TEXT     │       │ embedding  vector    │
                             │ content_hash  BIGINT   │       │            (384)     │
                             │ line_count    INTEGER  │       └──────────────────────┘
                             │ truncated     BOOLEAN  │        UNIQUE(file_id, chunk_index)
                             │ indexed_at    TZ       │
                             │ modified_at   TZ       │
                             └────────────────────────┘
```

**Indices:**

| Index                    | Type                             | Purpose                               |
|--------------------------|----------------------------------|---------------------------------------|
| `idx_chunks_embedding`   | HNSW (m=24, ef_construction=200) | Cosine similarity for semantic search |
| `idx_files_fts`          | GIN (tsvector)                   | Full-text search on file content      |
| `idx_files_path_trgm`    | GIN (pg_trgm)                    | Trigram similarity for path matching  |
| `idx_files_content_hash` | B-tree                           | Fast skip-if-unchanged lookups        |
| `idx_files_project`      | B-tree                           | Filter files by project               |
| `idx_files_language`     | B-tree                           | Filter files by language              |

---

## Installation

### Prerequisites

- **Rust** (2024 edition, nightly or stable 1.85+)
- **PostgreSQL 15+** with the [pgvector](https://github.com/pgvector/pgvector) and `pg_trgm` extensions
- ~500 MB disk for the all-MiniLM-L6-v2 ONNX model (downloaded on first run)

### Build

```bash
cargo build --release
```

With CUDA GPU acceleration (requires ONNX Runtime CUDA provider):

```bash
cargo build --release --features cuda
```

### Install

```bash
cp target/release/pgmcp /usr/local/bin/
```

### Database Setup

```sql
CREATE DATABASE pgmcp;
CREATE USER pgmcp WITH PASSWORD 'your_password';
GRANT ALL PRIVILEGES ON DATABASE pgmcp TO pgmcp;

-- Connect to the pgmcp database, then:
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

pgmcp runs migrations automatically on startup -- the extensions just need to exist.

---

## Configuration

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

[[indexer.file_types]]
extension = "rs"
language = "rust"

[[indexer.file_types]]
extension = "py"
language = "python"

# ... 15 file types configured by default

[indexer]
exclude_patterns = ["node_modules", "target", ".git", "__pycache__", "*.lock"]

[database]
host = "localhost"
port = 5432
name = "pgmcp"
user = "pgmcp"
# password via PGMCP_DB_PASSWORD env var or:
# password = "secret"
max_connections = 20

[embeddings]
model = "all-MiniLM-L6-v2"
dimensions = 384
chunk_size_lines = 50
chunk_overlap_lines = 10
batch_size = 32
pool_size = 2
use_gpu = false

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

[cron]
stale_cleanup_interval_secs = 3600
integrity_check_interval_secs = 86400
stats_aggregation_interval_secs = 60
db_maintenance_interval_secs = 604800
```

### Configuration Reference

| Section      | Key                               | Default                             | Description                                                |
|--------------|-----------------------------------|-------------------------------------|------------------------------------------------------------|
| `workspace`  | `paths`                           | `[]`                                | Directories to watch and index                             |
| `indexer`    | `debounce_ms`                     | `300`                               | Debounce window for file events                            |
| `indexer`    | `max_file_size_bytes`             | `1048576`                           | Files larger than this are indexed without content storage |
| `indexer`    | `exclude_patterns`                | `[node_modules, target, .git, ...]` | Glob/substring patterns to skip                            |
| `database`   | `host`                            | `localhost`                         | PostgreSQL host                                            |
| `database`   | `port`                            | `5432`                              | PostgreSQL port                                            |
| `database`   | `name`                            | `pgmcp`                             | Database name                                              |
| `database`   | `user`                            | `pgmcp`                             | Database user                                              |
| `database`   | `password`                        | `None`                              | Database password (prefer `PGMCP_DB_PASSWORD` env var)     |
| `database`   | `max_connections`                 | `20`                                | Connection pool size                                       |
| `embeddings` | `model`                           | `all-MiniLM-L6-v2`                  | Sentence-transformer model name                            |
| `embeddings` | `dimensions`                      | `384`                               | Embedding vector dimensions                                |
| `embeddings` | `chunk_size_lines`                | `50`                                | Lines per chunk                                            |
| `embeddings` | `chunk_overlap_lines`             | `10`                                | Overlapping lines between chunks                           |
| `embeddings` | `batch_size`                      | `32`                                | Embedding batch size                                       |
| `embeddings` | `pool_size`                       | `2`                                 | Dedicated embedding threads                                |
| `embeddings` | `use_gpu`                         | `false`                             | Enable CUDA (requires `cuda` feature)                      |
| `mcp`        | `transport`                       | `stdio`                             | Transport mode                                             |
| `mcp`        | `host`                            | `127.0.0.1`                         | Daemon bind address                                        |
| `mcp`        | `port`                            | `3100`                              | Daemon port                                                |
| `work_pool`  | `min_threads`                     | `2`                                 | Minimum active workers                                     |
| `work_pool`  | `max_threads`                     | `0` (num_cpus)                      | Maximum workers                                            |
| `work_pool`  | `initial_threads`                 | `0` (min_threads)                   | Workers at startup                                         |
| `metrics`    | `http_enabled`                    | `true`                              | Enable Prometheus endpoint                                 |
| `metrics`    | `http_port`                       | `9464`                              | Metrics port                                               |
| `logging`    | `level`                           | `info`                              | Log level (trace, debug, info, warn, error)                |
| `logging`    | `rotation`                        | `daily`                             | Log rotation period                                        |
| `cron`       | `stale_cleanup_interval_secs`     | `3600`                              | Interval to remove deleted files from index                |
| `cron`       | `integrity_check_interval_secs`   | `86400`                             | Interval to clean up incomplete indexing                   |
| `cron`       | `stats_aggregation_interval_secs` | `60`                                | Interval to refresh stats counters                         |
| `cron`       | `db_maintenance_interval_secs`    | `604800`                            | Interval for VACUUM ANALYZE                                |

### Environment Variables

| Variable            | Description                                           |
|---------------------|-------------------------------------------------------|
| `PGMCP_DB_PASSWORD` | Database password (takes precedence over config file) |
| `PGMCP_CONFIG`      | Path to configuration file                            |

---

## Usage

### CLI Commands

```bash
pgmcp init       # Generate default config at ~/.config/pgmcp/config.toml
pgmcp serve      # Run in foreground (stdout logging, stdio MCP transport)
pgmcp daemon     # Run as daemon (file logging, HTTP MCP transport, sd-notify)
pgmcp stats      # Print statistics from the database
pgmcp reindex    # Clear the index and restart to re-index everything
pgmcp context    # Print project context for current directory (for hooks)
```

#### `pgmcp context`

Prints a markdown summary of the project matching the current working directory,
including file count, language breakdown, and file tree. Designed to be called by
Claude Code hooks to inject project context automatically.

| Flag      | Default  | Description                                       |
|-----------|----------|---------------------------------------------------|
| `--cwd`   | `$PWD`   | Working directory to find project for              |
| `--depth` | `3`      | Maximum depth for file tree                        |

### Running as a Daemon

#### systemd Service

Create `/etc/systemd/system/pgmcp.service`:

```ini
[Unit]
Description=pgmcp - PostgreSQL MCP File Indexer
After=postgresql.service
Requires=postgresql.service

[Service]
Type=notify
ExecStart=/usr/local/bin/pgmcp daemon
Restart=on-failure
RestartSec=5
User=pgmcp
Environment=PGMCP_DB_PASSWORD=your_password

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now pgmcp
```

#### Direct

```bash
PGMCP_DB_PASSWORD=secret pgmcp daemon &
```

The daemon binds to `127.0.0.1:3100` by default and serves MCP over Streamable
HTTP at `/mcp`.

### Configuring Claude Code CLI

#### HTTP Transport (daemon mode -- recommended)

```bash
claude mcp add --transport http pgmcp http://localhost:3100/mcp
```

Or add to `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "pgmcp": {
      "type": "http",
      "url": "http://localhost:3100/mcp"
    }
  }
}
```

#### stdio Transport (foreground mode -- debugging)

```bash
claude mcp add --transport stdio pgmcp /usr/local/bin/pgmcp serve
```

#### Verification

```bash
claude mcp list
# Should show: pgmcp (connected)
```

### Auto-RAG: Claude Code Hooks

pgmcp can automatically inject relevant context into every Claude Code session
and prompt via two hooks. No manual tool calls needed -- Claude sees project
structure on session start and semantically relevant code on every prompt.

#### SessionStart Hook

Runs `pgmcp context` when a Claude Code session begins. Injects a markdown
summary containing the project name, root path, file count, language breakdown,
and file tree. If the current directory doesn't match any indexed project, it
lists all available projects instead.

#### UserPromptSubmit Hook

Runs `~/.claude/hooks/pgmcp-rag.sh` on every user prompt. The script queries
the daemon's `POST /api/search` endpoint with the prompt text and injects up to
5 semantically relevant code snippets as context. Short prompts (< 30 characters)
are skipped to avoid noise from commands like "yes" or "continue". The request
has a 3-second timeout and fails gracefully if the daemon is unavailable.

#### Configuration

Add the following to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "/usr/local/bin/pgmcp context",
            "timeout": 10000
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/pgmcp-rag.sh",
            "timeout": 3000
          }
        ]
      }
    ]
  }
}
```

#### Hook Script

Place the following script at `~/.claude/hooks/pgmcp-rag.sh` and make it
executable (`chmod +x`):

```bash
#!/bin/bash
# pgmcp RAG hook — injects relevant indexed code into Claude's context
# Reads user prompt from stdin JSON, queries pgmcp daemon for relevant snippets

set -e

INPUT=$(cat)
PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty')

# Skip short prompts (commands like "yes", "continue", "ok")
if [ ${#PROMPT} -lt 30 ]; then
    exit 0
fi

# Query pgmcp daemon for semantically relevant code
RESULTS=$(curl -s -m 2 "http://localhost:3100/api/search" \
    -H 'Content-Type: application/json' \
    -d "{\"query\": $(echo "$PROMPT" | jq -Rs .), \"limit\": 5}" 2>/dev/null) || exit 0

# Check if we got results
RESULT_COUNT=$(echo "$RESULTS" | jq -r '.results | length // 0' 2>/dev/null) || exit 0
if [ "$RESULT_COUNT" -eq 0 ]; then
    exit 0
fi

# Format results as context
echo "## pgmcp: Relevant indexed code"
echo ""
echo "$RESULTS" | jq -r '.results[] | "### \(.file_path) (similarity: \(.similarity | tostring | .[0:4]))\n```\(.language)\n\(.chunk)\n```\n"' 2>/dev/null || exit 0

exit 0
```

Requires `jq` and `curl` on the system PATH.

### REST API

In daemon mode, pgmcp exposes two REST API endpoints alongside the MCP server.

#### `POST /api/search` -- Semantic Search

Search indexed files by conceptual similarity.

**Request body:**

```json
{
  "query": "error handling patterns",
  "limit": 5,
  "project": "pgmcp",
  "language": "rust"
}
```

All fields except `query` are optional. `limit` defaults to 5.

**Response:**

```json
{
  "results": [
    {
      "file_path": "/home/user/projects/pgmcp/src/error.rs",
      "chunk": "impl From<sqlx::Error> for PgmcpError { ... }",
      "similarity": 0.63,
      "language": "rust"
    }
  ]
}
```

**Example:**

```bash
curl -s http://localhost:3100/api/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "database connection pool", "limit": 3}'
```

#### `GET /api/context` -- Project Context

Retrieve project context for a given working directory.

**Query parameters:**

| Parameter | Required | Default | Description                           |
|-----------|----------|---------|---------------------------------------|
| `cwd`     | Yes      | --      | Working directory to find project for |
| `depth`   | No       | `3`     | Maximum depth for file tree           |

**Response (project found):**

```json
{
  "found": true,
  "project": {
    "name": "pgmcp",
    "path": "/home/user/projects/pgmcp",
    "file_count": 49,
    "last_scanned": "2026-03-07 12:00:00 UTC",
    "languages": [
      {"language": "rust", "count": 46},
      {"language": "markdown", "count": 2}
    ],
    "tree": ["Cargo.toml", "README.md", "src/main.rs"]
  }
}
```

**Response (project not found):**

```json
{
  "found": false,
  "project": null,
  "indexed_projects": [
    {"name": "pgmcp", "path": "/home/user/projects/pgmcp", "file_count": 49}
  ]
}
```

**Example:**

```bash
curl -s 'http://localhost:3100/api/context?cwd=/home/user/projects/pgmcp&depth=3'
```

---

## MCP Capabilities

pgmcp advertises 5 of 8 MCP capabilities:

| Capability      | Description                                                                   |
|-----------------|-------------------------------------------------------------------------------|
| **Tools**       | 9 tools for searching, reading, and managing the index                        |
| **Resources**   | 2 static resources + 3 resource templates with URI parameters                 |
| **Completions** | Auto-completion for resource template parameters (`{name}`, `{path}`)         |
| **Logging**     | Server-to-client log push with dynamic verbosity control via `set_level()`    |
| **Tasks**       | Long-running async operations (reindex) with progress tracking & cancellation |

## MCP Tools Reference

pgmcp exposes 9 tools via the Model Context Protocol:

| Tool              | Parameters                                                             | Description                                              |
|-------------------|------------------------------------------------------------------------|----------------------------------------------------------|
| `semantic_search` | `query` (string), `limit?` (int, default 10), `language?` (string)     | Search by conceptual similarity using vector embeddings  |
| `text_search`     | `query` (string), `limit?` (int, default 10), `language?` (string)     | Full-text keyword search with TF-IDF ranking             |
| `grep`            | `pattern` (regex string), `glob?` (string), `limit?` (int, default 10) | Regex pattern search across file contents                |
| `read_file`       | `path` (string)                                                        | Read the content of an indexed file by absolute path     |
| `list_projects`   | *(none)*                                                               | List all discovered projects with file counts            |
| `project_tree`    | `project` (string), `depth?` (int, default 5)                          | Show the file tree for a project                         |
| `file_info`       | `path` (string)                                                        | Get metadata (size, language, line count, timestamps)    |
| `index_stats`     | *(none)*                                                               | Get overall indexing statistics and pool state           |
| `reindex`         | *(none)*                                                               | Clear the index; background scanner re-indexes all files |

### MCP Resources

| URI                | Description                        |
|--------------------|------------------------------------|
| `pgmcp://stats`    | Current indexing statistics (JSON) |
| `pgmcp://projects` | List of indexed projects (JSON)    |

### MCP Resource Templates

| URI Template                  | Parameter | Completable | Description                  |
|-------------------------------|-----------|-------------|------------------------------|
| `pgmcp://project/{name}`      | `name`    | Yes         | Project details by name      |
| `pgmcp://project/{name}/tree` | `name`    | Yes         | File tree for a project      |
| `pgmcp://file/{path}`         | `path`    | Yes         | Read an indexed file by path |

### Logging

The server pushes log messages to connected clients at the configured verbosity level.
Clients can change the level at any time via `logging/setLevel` (one of: `debug`, `info`,
`notice`, `warning`, `error`, `critical`, `alert`, `emergency`). Log events include
indexer progress, errors, and reindex lifecycle.

### Tasks

The `reindex` tool can be invoked as a long-running task via `tools/call` with the task
field set. The server returns a task ID immediately and processes the operation
asynchronously. Clients can poll `tasks/get` for progress, retrieve results via
`tasks/result`, list all tasks with `tasks/list`, or cancel with `tasks/cancel`.

---

## Monitoring

### Prometheus Metrics

When `metrics.http_enabled = true` (default), pgmcp exposes a Prometheus-compatible
endpoint at `http://127.0.0.1:9464/metrics`.

**Exported metrics:**

| Metric                    | Type    | Description                 |
|---------------------------|---------|-----------------------------|
| `pgmcp_files_indexed`     | counter | Total files indexed         |
| `pgmcp_files_failed`      | counter | Total files failed to index |
| `pgmcp_chunks_embedded`   | counter | Total chunks embedded       |
| `pgmcp_bytes_processed`   | counter | Total bytes processed       |
| `pgmcp_mcp_requests`      | counter | Total MCP requests served   |
| `pgmcp_mcp_errors`        | counter | Total MCP errors            |
| `pgmcp_semantic_searches` | counter | Semantic search count       |
| `pgmcp_text_searches`     | counter | Text search count           |
| `pgmcp_grep_searches`     | counter | Grep search count           |
| `pgmcp_active_threads`    | gauge   | Active work pool threads    |
| `pgmcp_queue_depth`       | gauge   | Work pool queue depth       |
| `pgmcp_uptime_seconds`    | gauge   | Server uptime               |

### CLI Stats

```bash
pgmcp stats
```

Prints a summary of indexed files, projects, chunks, and bytes from the database.

---

## Maintenance Jobs

pgmcp runs four automated maintenance jobs via a lock-free cron state machine:

| Job                 | Default Interval | Description                                                              |
|---------------------|------------------|--------------------------------------------------------------------------|
| `stale-cleanup`     | 1 hour           | Remove files from the index that no longer exist on disk                 |
| `integrity-check`   | 24 hours         | Delete files with `NULL` content_hash (incomplete indexing from crashes) |
| `stats-aggregation` | 60 seconds       | Refresh in-memory statistics counters from the database                  |
| `db-maintenance`    | 7 days           | Run `VACUUM ANALYZE` on `indexed_files` and `file_chunks`                |

All intervals are configurable in the `[cron]` section of the config file.

---

## Testing

```bash
# Unit tests + property-based tests (61 tests, no external dependencies)
cargo test --bin pgmcp

# Integration tests (requires Docker with PostgreSQL + pgvector)
cargo test --test integration -- --ignored

# MCP protocol tests (requires running PostgreSQL + built binary)
cargo test --test mcp_protocol -- --ignored
```

---

## License

Copyright 2026 Dylon Edwards

Licensed under the Apache License, Version 2.0. See [LICENSE.txt](LICENSE.txt) for
the full license text.
