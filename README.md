# **pgmcp**

### Software Engineering Intelligence Platform

pgmcp continuously indexes source code into PostgreSQL with vector embeddings,
then applies dependency graph analysis, topic clustering, architecture metrics,
and heuristic risk prediction to surface actionable engineering intelligence --
all exposed through 71 [Model Context Protocol](https://modelcontextprotocol.io/)
tools that Claude Code, Codex CLI, or any MCP client can call.

Three layers working together: a **real-time indexing engine** that watches
your file system and maintains a searchable mirror in PostgreSQL, an **automated
analysis pipeline** that builds dependency graphs, discovers code topics, and
computes quality metrics in the background, and a **tool interface** that lets
AI assistants query any of it on demand.

---

## Documentation map

The long-form reference docs are organised by purpose under [`docs/`](docs/):

| Topic | File |
|---|---|
| Complete MCP tool catalogue (all 71 tools, 9 tiers) | [`docs/tool-catalog.md`](docs/tool-catalog.md) |
| Implementation architecture (indexer / analysis / MCP surfaces) | [`docs/architecture.md`](docs/architecture.md) |
| Four query interfaces (semantic / FTS / regex / hybrid) | [`docs/search-modes.md`](docs/search-modes.md) |
| Daemon lifecycle, cron jobs, systemd integration | [`docs/operations.md`](docs/operations.md) |
| Agent integration (Claude Code, Codex, Papers, .pgmcp.toml) | [`docs/integration.md`](docs/integration.md) |
| Git history indexing (opt-in per project) | [`docs/git-indexing.md`](docs/git-indexing.md) |
| PostgreSQL schema reference | [`docs/schema.md`](docs/schema.md) |
| Configuration reference (config.toml + .pgmcp.toml + env) | [`docs/configuration.md`](docs/configuration.md) |
| REST API endpoints (alongside MCP) | [`docs/rest-api.md`](docs/rest-api.md) |
| MCP capabilities (Tools, Resources, Completions, Logging, Tasks) | [`docs/mcp-capabilities.md`](docs/mcp-capabilities.md) |
| Prometheus metrics + adaptive thread pool + pipeline observability | [`docs/monitoring.md`](docs/monitoring.md) |

Design-track documentation (forward-looking architectural designs):

- [`docs/memory-server/`](docs/memory-server/) — SOTA memory-server extension
- [`docs/decisions/`](docs/decisions/) — short ADRs (pgvector HNSW choice, memory-server design)
- [`docs/scientific-ledger/`](docs/scientific-ledger/) — incident write-ups (OOM fix, recovery times, etc.)
- [`docs/DEVELOPING.md`](docs/DEVELOPING.md) — local development setup

---

## Quick Start

### Prerequisites

- **Rust** (2024 edition, stable 1.85+)
- **PostgreSQL 15+** with [pgvector](https://github.com/pgvector/pgvector) and `pg_trgm` extensions
- **CUDA toolkit 12+** with `nvcc` on PATH, plus an NVIDIA GPU
- **AOCL-BLIS** (for ndarray BLAS; on Arch: `pacman -S aocl-blis`)
- Model cache space for Candle/Hugging Face embedding weights downloaded on first run

**Optional**: to index `~/Papers/` and `~/Documents/`, install `pdftotext` (poppler),
`ps2ascii` (ghostscript), and `pandoc`. See
[`docs/integration.md`](docs/integration.md) for per-platform install hints.

### Build & Install

CUDA is mandatory; `Cargo.toml` has no crate feature flags and there is no
CPU-only build mode. `build.rs` invokes `nvcc` to compile
`src/fcm/cuda/kernels.cu` into PTX at build time.

```bash
cargo build --release
cp target/release/pgmcp /usr/local/bin/
```

See [`docs/DEVELOPING.md`](docs/DEVELOPING.md) for the full verification
checklist (`./scripts/verify.sh`) and the pre-push hook setup.

### Database Setup

```sql
CREATE DATABASE pgmcp;
CREATE USER pgmcp WITH PASSWORD 'your_password';
GRANT ALL PRIVILEGES ON DATABASE pgmcp TO pgmcp;
-- Connect to the pgmcp database, then:
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

pgmcp runs migrations automatically on startup — the extensions just need to exist.

### Initialize & Run

```bash
pgmcp init                              # Generate ~/.config/pgmcp/config.toml
# Edit config.toml: set workspace.paths
PGMCP_DB_PASSWORD=secret pgmcp serve    # Foreground (stdout logging)
```

For systemd daemon mode see [`docs/operations.md`](docs/operations.md).

### Connect Claude Code or Codex

```bash
claude mcp add --transport http pgmcp http://localhost:3100/mcp
codex mcp add pgmcp --url http://localhost:3100/mcp
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

---

## Usage

```bash
pgmcp init                       # Generate default config
pgmcp upgrade-configs            # Upgrade global + all project configs
pgmcp serve                      # Foreground (stdio MCP)
pgmcp daemon                     # Daemon (HTTP MCP)
pgmcp stats                      # Stats from the database
pgmcp reindex                    # Clear + re-index everything
pgmcp context                    # Print project context (for hooks)
pgmcp init-project               # Create .pgmcp.toml in $PWD
pgmcp upgrade-project            # Merge new defaults into existing .pgmcp.toml
pgmcp analyze                    # Run all analysis jobs
pgmcp results                    # Print cached analysis results
pgmcp tool                       # List all MCP tools
pgmcp tool <name> [KEY=VALUE]    # Run any MCP tool from the CLI
```

Detailed flag reference and `pgmcp context`-for-hooks integration:
[`docs/integration.md`](docs/integration.md). REST surface:
[`docs/rest-api.md`](docs/rest-api.md).

---

## Testing

```bash
cargo test --bin pgmcp                          # Unit + property tests
cargo test --release -p pgmcp-testing           # Integration tests
./scripts/verify.sh                             # 8-gate verification (must pass before commit)
```

Tier-C tests that hit a real Postgres self-skip when `PGMCP_TEST_DATABASE_URL`
is unset; set it (and ensure pgvector is loaded) to exercise the full suite.

The pre-push hook at `.githooks/pre-push` runs `./scripts/verify.sh` on every
push; activate it once per clone:

```bash
git config core.hooksPath .githooks
```

See `CLAUDE.md` for the agent-facing version of these rules and
[`docs/DEVELOPING.md`](docs/DEVELOPING.md) for the developer-facing one.

---

## License

Copyright 2026 Dylon Edwards. Licensed under the Apache License,
Version 2.0. See [LICENSE.txt](LICENSE.txt) for the full license text.
