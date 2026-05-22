# pgmcp REST API

Operator-facing HTTP endpoints alongside the MCP surface. For the MCP
tool catalog see [tool-catalog.md](tool-catalog.md); for daemon
lifecycle see [operations.md](operations.md).


In daemon mode, pgmcp exposes a small REST surface alongside the MCP server.
Endpoints (registered at `src/cli/daemon.rs`):

| Endpoint                  | Method | Purpose                                                                                                          |
|---------------------------|--------|------------------------------------------------------------------------------------------------------------------|
| `/health`                 | GET    | Cheap liveness probe (no DB queries). 200 when daemon is `Ready`, 503 otherwise.                                 |
| `/api/search`             | POST   | Semantic search; embeds query, runs vector ranking. Used by `~/.claude/hooks/pgmcp-rag.sh`.                      |
| `/api/context`            | GET    | Project context for a working directory; used by `pgmcp context` CLI and the SessionStart hook.                  |
| `/api/status`             | GET    | Rich status snapshot — daemon phase, pool state, embeddings config, indexing counters, MCP session counts, etc.  |
| `/api/grep`               | POST   | Cross-project regex grep. Used by `~/.claude/hooks/pgmcp-grep-companion.sh`.                                     |
| `/api/file_envelope`      | POST   | File metadata envelope (language, line count, last_indexed_at). Used by `~/.claude/hooks/pgmcp-read-context.sh`. |

The MCP server is also mounted at `/mcp` (Streamable HTTP transport). All
endpoints share a single Axum router and an `ApiState` that includes the
`DbClient`, query embedder, config, stats tracker, and `DaemonLifecycle`.

### `GET /health` -- Cheap Liveness Probe

Sub-millisecond probe that reads only the atomic `DaemonPhase` — no DB queries,
no model touch. Designed to be polled at high frequency by k8s probes, systemd
watchdogs, uptime monitors, and the `~/.claude/hooks/pgmcp-*.sh` PreToolUse
hooks (which check it with a 300 ms timeout to fail-fast on daemon outage).

| Phase          | HTTP Status         | Body                              |
|----------------|---------------------|-----------------------------------|
| `Ready`        | 200 OK              | `{"phase": "ready"}`              |
| `Initializing` | 503 SERVICE_UNAVAIL | `{"phase": "initializing"}`       |
| `Scanning`     | 503 SERVICE_UNAVAIL | `{"phase": "scanning"}`           |
| `Terminating`  | 503 SERVICE_UNAVAIL | `{"phase": "terminating"}`        |
| `Defunct`      | 503 SERVICE_UNAVAIL | `{"phase": "defunct"}`            |

**Example:**

```bash
curl -i -m 0.3 http://localhost:3100/health
# HTTP/1.1 200 OK
# {"phase":"ready"}
```

Distinct from `/api/status`, which returns rich state but issues ~10 SQL
`COUNT(*)` queries — appropriate for occasional inspection, not high-frequency
liveness polling.

### `POST /api/search` -- Semantic Search

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

### `GET /api/context` -- Project Context

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

### `GET /api/status` -- Daemon Status Snapshot

Comprehensive snapshot of daemon state, indexing progress, pool capacity,
embeddings configuration, and live counters. Issues ~10 cheap SQL `COUNT(*)`
queries plus an atomic snapshot of `StatsTracker`.

**Response (abridged):**

```json
{
  "daemon": {
    "version": "0.1.0",
    "uptime_secs": 3600,
    "current_rss_bytes": 524288000,
    "peak_rss_bytes": 1073741824,
    "heavy_cron_running": false,
    "http_mcp_sessions": 1,
    "bind_addr": "127.0.0.1:3100"
  },
  "database": { "host": "localhost", "port": 5432, "pool_size": 10, "pool_active": 2, ... },
  "embeddings": { "model": "all-MiniLM-L6-v2", "dimensions": 384, "backend": "candle", "device": "cuda:0", ... },
  "pools": { "inference": {...}, "cron": {...}, "general": {...} },
  "model_state": { "project_count": 14, "indexed_file_count": 21847, "chunk_count": 92418, ... },
  "counters": {
    "files_indexed": 21847,
    "semantic_searches": 142,
    "tool_invocations": {
      "semantic_search": 142,
      "grep": 23,
      "orient": 8,
      "...": "..."
    }
  }
}
```

The `counters.tool_invocations` map is populated by `StatsTracker::record_tool_call()`
at the top of each `#[tool]` body — useful for A/B-testing utilization
(see [pgmcp Utilization](#pgmcp-utilization-claude-code-integration) below).

### `POST /api/grep` -- Cross-Project Regex Grep

Server-side regex search across all indexed files. Used by the
`~/.claude/hooks/pgmcp-grep-companion.sh` PreToolUse hook to inject cross-project
hits alongside the native `Grep` tool's output.

**Request body:**

```json
{
  "pattern": "FcmBackend",
  "glob": "*.rs",
  "limit": 10
}
```

`glob` and `limit` are optional. `limit` clamped to `[1, 50]`, default 10.

**Response:**

```json
{
  "results": [
    {
      "path": "/home/dylon/Workspace/f1r3fly.io/pgmcp/src/fcm/cpu.rs",
      "relative_path": "src/fcm/cpu.rs",
      "language": "rust",
      "content": "impl FcmBackend for CpuFcmBackend { ... }"
    }
  ],
  "truncated": false
}
```

`truncated` is true when `results.len() == limit` (more matches available).

### `POST /api/file_envelope` -- File Metadata Envelope

Compact metadata for a specific path: language, line count, indexed_at, etc.
Used by `~/.claude/hooks/pgmcp-read-context.sh` to inject a one-line context
block alongside any `Read` tool call.

**Request body:**

```json
{ "path": "/home/dylon/Workspace/f1r3fly.io/pgmcp/src/lib.rs" }
```

**Response (file in index):**

```json
{
  "found": true,
  "info": {
    "path": "/home/dylon/Workspace/f1r3fly.io/pgmcp/src/lib.rs",
    "relative_path": "src/lib.rs",
    "language": "rust",
    "size_bytes": 1234,
    "line_count": 42,
    "truncated": false,
    "indexed_at": "2026-04-28T12:34:56Z",
    "modified_at": "2026-04-28T12:30:00Z"
  }
}
```

**Response (file not in index — e.g., just written or `.gitignore`'d):**

```json
{ "found": false, "info": null }
```

---

