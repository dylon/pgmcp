# **pgmcp**

### Software Engineering Intelligence Platform

pgmcp continuously indexes source code into PostgreSQL with vector embeddings,
then applies dependency graph analysis, topic clustering, architecture metrics,
and heuristic risk prediction to surface actionable engineering intelligence --
all exposed through 40 [Model Context Protocol](https://modelcontextprotocol.io/)
tools that Claude Code, Codex CLI, or any MCP client can call.

Think of it as three layers working together: a **real-time indexing engine** that
watches your file system and maintains a searchable mirror in PostgreSQL, an
**automated analysis pipeline** that builds dependency graphs, discovers code
topics, and computes quality metrics in the background, and a **tool interface**
that lets AI assistants query any of it on demand.

---

## Capability Overview

pgmcp's 71 MCP tools are organized into nine capability tiers:

### Search & Retrieval (6 tools)

Find code by meaning, keywords, regex, or fused ranking across all indexed projects.

| Tool              | Description                                                                                         |
|-------------------|-----------------------------------------------------------------------------------------------------|
| `semantic_search` | Vector similarity search (cosine over HNSW). Use `project: "claude"` or `project: "codex"` to search agent session transcripts. |
| `text_search`     | PostgreSQL full-text search with BM25/TF-IDF ranking                                                |
| `grep`            | Server-side regex across file contents with optional glob filtering                                 |
| `hybrid_search`   | Reciprocal Rank Fusion of BM25 + vector search with configurable weights                            |
| `read_file`       | Read the content of an indexed file by path                                                         |
| `search_commits`  | Semantic search over git commit history (messages + diffs)                                          |

### Software Pattern Knowledge (8 tools)

Design with a separate, local full-text pattern/anti-pattern index. The catalog
holds **~810 entries spanning 14 paradigms** — patterns, anti-patterns,
principles (SOLID, GRASP, DRY/KISS/YAGNI, package principles, …), and code
smells (Long Method, Feature Envy, Shotgun Surgery, …) — across object-oriented,
functional, logic, event-driven, concurrent, parallel, aspect-oriented,
distributed, reactive, dataflow, declarative, actor-model, procedural, and
machine-learning engineering. Round 2 added dense coverage of observability /
SRE, deployment & release, data engineering, API design, ML/AI engineering
(RAG, vector search, chain-of-thought, ReAct, function calling), CRDTs and
distributed-data primitives (G-Counter, Vector Clock, Bloom filter, HLL),
Kubernetes patterns (Operator, CRD, HPA, NetworkPolicy), plus more Fowler
refactorings and security primitives (AEAD, forward secrecy, PKCE, HSTS). These tools use the same embedding model as file search, but never
query `file_chunks`.

| Tool                         | Description                                                                 |
|------------------------------|-----------------------------------------------------------------------------|
| `software_pattern_search`    | Semantic search over software patterns, anti-patterns, principles, code smells, paradigms, and sources |
| `recommend_design_patterns`  | Recommend patterns, principles, anti-patterns, and code smells for a feature/refactor task |
| `review_design_patterns`     | Review a proposed design for anti-pattern risks, code-smell hits, principle reminders, and better alternatives |
| `get_software_pattern`       | Fetch one pattern card with source links and optional bounded excerpts       |
| `list_software_patterns`     | Browse/filter the catalog by paradigm, kind (`pattern`/`anti_pattern`/`principle`/`code_smell`), category, or source |
| `pattern_catalog_stats`      | Per-kind counts (patterns / anti-patterns / principles / code-smells), source/chunk/import status |
| `refresh_pattern_catalog`    | Seed, import, or incrementally refresh local full-text pattern sources       |
| `upsert_pattern_source`      | Attach local full-text docs/snippets to an existing pattern                  |

### Project Intelligence (6 tools)

Discover, navigate, and manage indexed projects.

| Tool            | Description                                                                                                                                                                              |
|-----------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `orient`        | Composite first-step snapshot: project metadata + language breakdown + depth-2 tree + key entry points by PageRank + recently-changed files + top topics + health envelope, in one call |
| `list_projects` | List all discovered projects with file counts                                                                                                                                            |
| `project_tree`  | Show the file tree for a project at configurable depth                                                                                                                                   |
| `file_info`     | Get metadata (size, language, line count, timestamps) for a file                                                                                                                         |
| `index_stats`   | Overall indexing statistics, pool state, and analysis counters                                                                                                                           |
| `reindex`       | Clear the index and re-index all workspaces (async task with progress)                                                                                                                   |

`orient` is the recommended first call when entering an unfamiliar codebase or
starting a non-trivial task. It bundles what the model would otherwise spread
across `list_projects` + `project_tree` + `centrality_analysis` + recently-changed
queries, with a `health` envelope flagging stale graph metrics or missing topic
data so callers can interpret partial results correctly.

### Cross-Project Similarity (4 tools)

Find duplicated code and refactoring opportunities across your entire workspace.

| Tool                   | Description                                                                            |
|------------------------|----------------------------------------------------------------------------------------|
| `compare_files`        | Real-time chunk-level vector comparison of two specific files                          |
| `find_similar_modules` | Find similar files across projects (from materialized similarity table)                |
| `find_duplicates`      | Union-find clustering of duplicated code spanning multiple projects                    |
| `refactoring_report`   | Actionable refactoring candidates with suggested crate names and shared line estimates |

### Topic Clustering & Code Patterns (9 tools)

Discover semantic code patterns using Fuzzy C-Means clustering with c-TF-IDF keyword labeling.

| Tool                  | Description                                                            |
|-----------------------|------------------------------------------------------------------------|
| `discover_topics`     | Run FCM topic clustering (real-time per-project or cached global)      |
| `find_orphans`        | Identify chunks/files with low topic membership (dead code candidates) |
| `find_misplaced_code` | Detect files whose content doesn't match their directory context       |
| `find_coupled_files`  | Co-change coupling from git history (Jaccard similarity)               |
| `test_coverage_gaps`  | Topics with implementation code but no corresponding tests             |
| `complexity_hotspots` | Composite complexity ranking (size, chunks, topics, coupling)          |
| `topic_hierarchy`     | Agglomerative clustering showing how topics relate hierarchically      |
| `suggest_merges`      | Files covering overlapping topics that should be consolidated          |
| `suggest_splits`      | Files spanning too many topics, with suggested split points            |

### Dependency Graph Analysis (5 tools)

Build and query the import dependency graph using PageRank, Louvain, and Tarjan's SCC.

| Tool                     | Description                                                                |
|--------------------------|----------------------------------------------------------------------------|
| `dependency_graph`       | Visualize import relationships (summary, edge list, or DOT format)         |
| `centrality_analysis`    | Rank files by PageRank, betweenness centrality, or degree                  |
| `community_detection`    | Louvain community detection vs. directory structure alignment              |
| `circular_dependencies`  | Find dependency cycles via Tarjan's SCC + DFS cycle extraction             |
| `change_impact_analysis` | Predict affected files from import graph + co-change + semantic similarity |

### Architecture & Design Quality (6 tools)

Measure architecture health using Robert C. Martin's package metrics, design smells, and Card & Glass complexity.

| Tool                       | Description                                                            |
|----------------------------|------------------------------------------------------------------------|
| `coupling_cohesion_report` | Ca/Ce/Instability/Abstractness/D* per module (Martin's metrics)        |
| `architecture_violations`  | Cycles, god modules, SDP violations, zone of pain/uselessness          |
| `design_smell_detection`   | God class, SRP violation, shotgun surgery, stale modules               |
| `architecture_quality`     | 10-dimension positive quality measurement (0-100% per dimension)       |
| `design_metrics`           | Card & Glass S/D/Sy, cyclomatic complexity, WMC, maintainability index |
| `doc_coverage_gaps`        | Code topics lacking corresponding documentation                        |

### Risk & Health Prediction (3 tools)

Identify high-risk files using heuristic composite scoring over structural and historical metrics.

| Tool                      | Description                                                            |
|---------------------------|------------------------------------------------------------------------|
| `bug_prediction`          | Composite bug-proneness: churn x complexity x fix_ratio x coupling     |
| `technical_debt_analysis` | TODO density + complexity + test gaps + D* + churn = debt score        |
| `anomaly_detection`       | Embedding distance from centroid + metric z-scores = outlier detection |

### Summarization & Scorecard (2 tools)

High-level project understanding and engineering quality assessment.

| Tool                    | Description                                                     |
|-------------------------|-----------------------------------------------------------------|
| `code_summarize`        | Topic-based structural summary of a project, directory, or file |
| `engineering_scorecard` | A-F grades across 10 dimensions, GPA, and ORR checklist         |

### Infrastructure Features

- **Real-time file watching** -- `notify` crate with debounced event processing
- **Adaptive thread pool** -- hill-climbing autoscaler with exponential moving averages
- **Unified embed pool** -- dual-channel workers with priority query channel (~90 MB saved vs. separate model)
- **Daemon lifecycle** -- Initializing -> Scanning -> Ready -> Terminating -> Defunct (heavy jobs gate on Ready)
- **Lock-free reactive pipeline** -- crossbeam channels for zero-mutex data flow
- **Two-phase commit** -- atomic indexing with content hash finalization
- **Incremental indexing** -- xxHash3 content hashing skips unchanged files
- **Streamable HTTP transport** -- multi-client daemon mode for shared team indexing
- **systemd integration** -- `sd-notify` ready/stopping protocol
- **17 file types** -- Rust, Python, TypeScript, JavaScript, Go, Rholang, MeTTa, Prolog, Shell, JSONL, Markdown, and more
- **Per-project overrides** -- `.pgmcp.toml` in project roots for custom exclusions and file types
- **CUDA acceleration** -- mandatory GPU-accelerated embedding and FCM paths via Candle/cudarc
- **Cross-agent memory search** -- synthetic `claude` and `codex` projects make both clients' config, prompt history, and sessions queryable through the same MCP tools
- **Software pattern knowledge index** -- separate pgvector tables for local full-text design pattern/anti-pattern sources; file search tools never return pattern docs
- **Auto-RAG context injection** -- Claude Code hooks inject project context and relevant code on every prompt
- **PreToolUse tool-call proxy** -- five hook scripts at `~/.claude/hooks/pgmcp-*.sh` augment (Layer A) or selectively deny (Layer B, opt-in via `PGMCP_HOOK_MODE=enforce`) `Read`/`Grep`/`Glob` to bias Claude toward pgmcp's richer tools
- **Subagent containment** -- `~/.claude/agents/Explore.md` and `general-purpose.md` overrides drop `Grep`/`Glob` from spawned-subagent tool catalogs (harness-enforced; subagents do not inherit parent `PreToolUse` hooks)
- **REST API** -- `/health`, `/api/search`, `/api/context`, `/api/status`, `/api/grep`, `/api/file_envelope` alongside the MCP server
- **Per-tool timeout wrapping** -- every non-reindex `#[tool]` body wrapped in `tokio::time::timeout(30s, ...)`; clients see structured `McpError` instead of hanging connections on stuck tools
- **Per-tool invocation counters** -- `StatsTracker::tool_invocations` `DashMap` for utilization A/B-testing
- **Prometheus metrics** -- `/metrics` endpoint + `pgmcp stats` CLI

---

## Architecture Overview

```
                       ┌──────────────────────────────────────────────────────┐
                       │                      pgmcp daemon                    │
                       │                                                      │
                       │  ┌── Interface Layer ───────┬───────────┬─────────┐  │
                       │  │  MCP Server (71 tools)   │  REST API │   CLI   │  │
                       │  └───────────┬──────────────┴───────────┴─────────┘  │
                       │              │                                       │
                       │  ┌── Analysis Layer ──────────┬──────┬────────────┐  │
                       │  │  Graph Module      Topic   │ Similarity        │  │
                       │  │  (PageRank,        Clust.  │ Scanner           │  │
                       │  │   Louvain,         (FCM,   │ (HNSW batch)      │  │
                       │  │   Tarjan SCC,      c-TF-   │                   │  │
                       │  │   Martin           IDF)    │                   │  │
                       │  │   metrics)                 │                   │  │
                       │  └────────────┬───────────────┴───────────────────┘  │
                       │               │                                      │
                       │  ┌── Data Layer ──────────────────────────────────┐  │
                       │  │  PostgreSQL + pgvector (12 tables)             │  │
                       │  │  HNSW indices, GIN FTS, content hashing        │  │
                       │  │                                                │  │
  ┌───────────┐ notify │  │  ┌── Indexing Pipeline ─────────────────────┐  │  │
  │ File      ├─events→│  │  │ Watcher → Filter → Debounce → WorkPool   │  │  │
  │ System    │        │  │  │   → Chunker → Embedding Pool → DB        │  │  │
  └───────────┘        │  │  └──────────────────────────────────────────┘  │  │
                       │  └────────────────────────────────────────────────┘  │
                       │                                                      │
                       └─────┬────────────────────────────────────┬───────────┘
                             │                                    │
            ┌────────────────┼────────┬───────────┐               │
            │                │        │           │               │
            ▼                ▼        ▼           ▼               ▼
   ┌────────────────┐ ┌────────────────┐ ┌──────────────────┐ ┌─────────────┐
   │ Claude Code    │ │ Claude Code    │ │ Claude Code      │ │ Prometheus  │
   │ (stdio)        │ │ (HTTP/MCP)     │ │ Hooks (auto-RAG) │ │ /metrics    │
   └────────────────┘ └────────────────┘ └──────────────────┘ └─────────────┘
```

**Key data flow:**

1. **File system events** flow through the watcher, get filtered by extension and
   exclusion patterns, then debounced by path (default 300 ms)
2. **Debounced events** are dispatched at **HIGH** priority to the adaptive WorkPool
3. **Bulk scan** paths enter at **LOW** priority -- live edits always take precedence
4. **Workers** read the file, compute xxHash3, check the DB for changes, chunk the
   content into overlapping windows, and submit chunks to the embedding pool
5. **Embedding workers** (each owning its own model instance) batch-embed chunks and
   upsert them with their vectors into PostgreSQL. A priority query channel serves
   MCP/API embedding requests ahead of bulk indexing.
6. **Two-phase commit:** the file's `content_hash` is set to `NULL` during
   processing and only finalized after all chunks succeed -- crash-safe by design
7. **Analysis cron jobs** (graph, topics, similarity) run in the background after the
   initial scan completes, populating derived tables that the analysis tools query
8. **MCP clients** query the index via any of 71 tools over stdio or Streamable HTTP

---

## Quick Start

### Prerequisites

- **Rust** (2024 edition, nightly or stable 1.85+)
- **PostgreSQL 15+** with [pgvector](https://github.com/pgvector/pgvector) and `pg_trgm` extensions
- **CUDA toolkit 12+** with `nvcc` on PATH, plus an NVIDIA GPU
- **AOCL-BLIS** (for ndarray BLAS used by deterministic CPU FCM tests; on Arch: `pacman -S aocl-blis`)
- Model cache space for Candle/Hugging Face embedding weights downloaded on first run

#### Optional — document indexing

To index `~/Papers/` (academic papers in PDF/LaTeX/EPUB) and `~/Documents/`
(notes, invoices, mixed formats), install the document-extraction CLI tools.
Without them, files of the affected types are skipped at index time and the
count is surfaced via `index_stats.documents_skipped_no_tool`.

| Tool        | Used for                              | Arch              | Debian/Ubuntu          | macOS                   |
|-------------|---------------------------------------|-------------------|------------------------|-------------------------|
| `pdftotext` | PDF text extraction                   | `poppler`         | `poppler-utils`        | `brew install poppler`  |
| `ps2ascii`  | PostScript / `.ps` / `.eps`           | `ghostscript`     | `ghostscript`          | `brew install ghostscript` |
| `pandoc`    | DOCX, DOC, RTF, ODT, EPUB, LaTeX, ORG | `pandoc-cli`      | `pandoc`               | `brew install pandoc`   |

These are looked up once at daemon startup; missing tools produce a single
`warn!` line per tool naming the install hint. Plain text and structured-text
formats (`.md`, `.txt`, `.rst`, `.bib`) need no extra tools.

### Build & Install

CUDA is mandatory. `Cargo.toml` has no crate feature flags and there is no
CPU-only build mode. Production compute paths fail closed if GPU initialization
fails; the CPU FCM backend exists for deterministic tests and diagnostics only.

```bash
cargo build --release
cp target/release/pgmcp /usr/local/bin/
```

`build.rs` invokes `nvcc` to compile `src/fcm/cuda/kernels.cu` into PTX at
build time. If `nvcc` is not on PATH, `cargo build` fails cleanly.

See `docs/DEVELOPING.md` for the full verification checklist
(`./scripts/verify.sh`) and the pre-push hook setup.

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

### Initialize & Run

```bash
pgmcp init                    # Generate config at ~/.config/pgmcp/config.toml
# Edit config.toml: set workspace.paths to your project directories
PGMCP_DB_PASSWORD=secret pgmcp serve   # Foreground mode (stdout logging)
```

### Connect Claude Code or Codex

```bash
claude mcp add --transport http pgmcp http://localhost:3100/mcp
claude mcp list   # Should show: pgmcp (connected)

codex mcp add pgmcp --url http://localhost:3100/mcp
codex mcp list    # Should show: pgmcp
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

For stdio transport (foreground debugging):

```bash
claude mcp add --transport stdio pgmcp /usr/local/bin/pgmcp serve
codex mcp add pgmcp -- /usr/local/bin/pgmcp serve
```

---

## Search Modes

pgmcp provides four complementary search strategies:

**Semantic Search** -- finds conceptually related code even when terminology differs.
The query is embedded into the same 384-dimensional vector space, then ranked by
cosine similarity via pgvector's HNSW index:

```
score(q, c) = 1 - cosine_distance(embed(q), embed(c))
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

**Hybrid Search** -- fuses text and semantic search using Reciprocal Rank Fusion
(RRF). Both search strategies run in parallel, then results are merged:

```
RRF_score = bm25_weight / (k + rank_text) + semantic_weight / (k + rank_vec)
```

where k = 60 (standard RRF constant). Results appearing in both lists get boosted;
results in only one list still contribute. Configurable `bm25_weight` and
`semantic_weight` (default 0.5 each).

---

## Automated Analysis & Daemon Lifecycle

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

## Agent Client Integration

### Auto-Discovery of Agent Homes

On startup, pgmcp checks whether `~/.claude/` exists and, if so, registers it as
a synthetic **"claude"** project. All indexable files within are scanned and indexed
just like any workspace project. This includes:

- Memory files (`CLAUDE.md`, project memory files)
- Plans and design documents
- Session transcript JSONL files (`projects/*/` session logs)
- Hook scripts and configuration files

A hardcoded `CLAUDE_DIR_EXCLUDES` list filters out noise directories (telemetry,
debug logs, cache, binary snapshots).

pgmcp also checks whether `~/.codex/` exists and registers it as a synthetic
**"codex"** project. Codex stores credentials, sqlite state, shell snapshots,
plugin checkouts, and caches in the same directory, so pgmcp uses an allow-list:
`config.toml`, `history.jsonl`, `memories/**`, and `sessions/**/*.jsonl`.

### Document Indexing — `~/Papers/` and `~/Documents/`

Beyond source code, pgmcp can index **personal document corpora**: academic
papers, notes, invoices, manuals — the kinds of things you'd want to grep or
semantic-search even though they aren't in a git repo.

On startup, pgmcp checks for two well-known document directories and registers
them as synthetic projects when they exist:

| Directory       | Project name | Typical contents                              |
|-----------------|--------------|-----------------------------------------------|
| `~/Papers/`     | `Papers`     | Academic PDFs, LaTeX source, EPUB textbooks   |
| `~/Documents/` | `Documents`  | Notes (ORG/MD/RST), invoices, DOCX, ODT, RTF  |

No `.git/` is required; the directory's mere existence enables it. Users
without these directories pay no cost (the daemon `is_dir()`-guards both).

#### Supported formats

| Extension      | Language       | Storage form                                 |
|----------------|----------------|----------------------------------------------|
| `pdf`          | `pdf`          | `pdftotext -layout`, NFKC + dehyphenated     |
| `ps` / `eps`   | `postscript`   | `ps2ascii`, NFKC                             |
| `docx`         | `docx`         | `pandoc --to plain`                          |
| `doc`          | `doc`          | `pandoc --to plain` (needs antiword/catdoc)  |
| `rtf`          | `rtf`          | `pandoc --to plain`                          |
| `odt`          | `odt`          | `pandoc --to plain`                          |
| `epub`         | `epub`         | `pandoc --to plain`                          |
| `tex`/`latex`  | `latex`        | `pandoc --to plain` (strips markup)          |
| `org`          | `org`          | `pandoc --to plain` (strips markup)          |
| `rst`          | `rst`          | UTF-8 passthrough + normalization            |
| `bib`          | `bibtex`       | UTF-8 passthrough + normalization            |
| `txt`          | `text`         | UTF-8 passthrough + normalization            |

The extraction layer routes binary formats through system tools (see
Prerequisites) and applies a single **normalization pass** to all outputs:
NFKC Unicode, dehyphenation of line-break-split words, page-number-line
stripping, control-character removal, whitespace collapse. The result is
stored verbatim in `file_chunks.content` so MCP tool responses are
already token-efficient — no separate wire format needed.

#### Source-form preference

When several forms of the same document coexist in one directory — e.g.
`invoice.org`, `invoice.tex`, `invoice.pdf` — pgmcp indexes **only the source
form**, not the build output. The default priority (configurable per project
in `.pgmcp.toml`) is:

```
org > rst > md > tex > latex > docx > epub > odt > rtf > pdf > ps > eps > doc
```

Files whose extension isn't in the priority list (e.g. `.csv`) are kept
unconditionally — they can't be deduplicated against anything.

#### Content-based dedup and rename detection

For document corpora, the same PDF often ends up in two places (download
folder + organized archive) or gets moved as the library is reorganized.
pgmcp detects both cases via content hashing **before** extraction:

- **Rename** — same content, different path, old path is gone on disk:
  the existing canonical row's path is updated in place; chunks and
  embeddings are reused.
- **Cross-path duplicate** — same content, different path, old path still
  present: insert a metadata-only row pointing at the canonical via
  `duplicate_of_file_id`. Chunk-bearing queries follow the pointer
  transparently via `COALESCE(duplicate_of_file_id, id)`.

The savings are large: moving or duplicating a 50-page PDF is now O(stat)
instead of triggering subprocess extraction + GPU embedding. Counters
`documents_renamed` and `documents_deduplicated` in `index_stats` surface
the impact.

#### Recommended agent workflow (token-efficient)

For documents projects, prefer chunk-level retrieval over file-level:

- `semantic_search project=Papers query="attention mechanism" limit=5` —
  ~5 chunks (~2-3k tokens) targeted at the question.
- `grep pattern="error budget" project=Documents before_context=2 after_context=2`
  — chunk-anchored regex matches with surrounding lines, ~500 tokens per
  hit instead of whole-file (~20-50k tokens).
- `read_file path=~/Papers/<sample>.pdf start_line=100 end_line=150` —
  pulls only the requested line range from indexed chunks, stitched and
  trimmed. Works even when `indexed_files.content` is NULL (Level-1
  oversized files): chunks are stitched on demand.
- `read_file path=~/Papers/<sample>.pdf chunk_index_start=5 chunk_index_end=6`
  — alternate chunk-indexed addressing for paging through long documents.

`file_info` reports `chunk_count`, `first/last_chunk_line`, and
`extracted_kind` (e.g. `pdf_text`, `docx_text`, `latex_plain`) so the agent
can plan further reads in one round-trip.

#### Per-project `.pgmcp.toml`

Drop a `.pgmcp.toml` into `~/Papers/` or `~/Documents/` to override defaults:

```toml
[indexer]
# Override the 1 MiB default for binary docs (default 100 MiB):
max_document_source_bytes = 209715200   # 200 MiB

# Per-project priority replacement (note: replace semantics, not merge):
source_priority = ["org", "tex", "latex", "rst", "md", "epub", "pdf", "ps", "eps"]

# Exclude LaTeX build artifacts in addition to the hardcoded defaults:
exclude_patterns = [
    "*.aux", "*.log", "*.out", "*.toc",
    "*.synctex.gz", "*.fls", "*.fdb_latexmk",
    "supplementary/", "submissions-archive/",
]

# Documents typically aren't in git; turn history indexing off explicitly.
[git]
index_history = false
```

### Project-Level `.claude/` Scanning

For each discovered project, pgmcp also scans its `.claude/` subdirectory (if
present). Files found there -- memory files, plans, session transcripts -- are
indexed as part of the parent project, so searches against that project include
its Claude Code context.

### Claude JSONL Session Transcript Parsing

Claude Code session transcripts are stored as JSONL files. pgmcp includes a
dedicated parser (`claude_chunker`) that extracts meaningful messages:

- **User messages** -- the prompts you sent
- **Assistant messages** -- Claude's responses (text content)
- **Tool results** -- output from tool calls

Each extracted message becomes a separate chunk with its own embedding, making
session history semantically searchable. Generic (non-Claude) JSONL files are
chunked one line per chunk.

### Codex JSONL Session and History Parsing

Codex session rollouts live under `~/.codex/sessions/YYYY/MM/DD/*.jsonl`, and
prompt history lives at `~/.codex/history.jsonl`. pgmcp extracts user messages,
assistant responses, tool calls, and bounded tool outputs while skipping
developer/system instructions, reasoning records, encrypted payloads, token
counts, invalid JSON lines, and oversized tool output.

Both synthetic projects live in the same PostgreSQL index. Claude can search
Codex history with `project: "codex"`, and Codex can search Claude history with
`project: "claude"`.

### Auto-RAG Hooks

pgmcp can automatically inject relevant context into every Claude Code session
and prompt via two hooks. No manual tool calls needed.

Codex CLI supports MCP server registration, so it can query pgmcp tools directly.
It does not currently expose Claude-style prompt hooks in the local CLI surface,
so automatic prompt-time injection is Claude-specific.

**SessionStart Hook** -- runs `pgmcp context` when a Claude Code session begins.
Injects a markdown summary containing the project name, root path, file count,
language breakdown, and file tree.

**UserPromptSubmit Hook** -- runs `~/.claude/hooks/pgmcp-rag.sh` on every user
prompt. Queries the daemon's `POST /api/search` endpoint with the prompt text and
injects up to 5 semantically relevant code snippets. Short prompts (< 30 chars)
are skipped. 2-second timeout with graceful fallback.

**Configuration** -- add to `~/.claude/settings.json`:

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

**Hook Script** -- place at `~/.claude/hooks/pgmcp-rag.sh` (`chmod +x`):

```bash
#!/bin/bash
# pgmcp RAG hook -- injects relevant indexed code into Claude's context
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

---

## pgmcp Utilization (Claude Code Integration)

The auto-RAG hook above enriches *every* prompt, but Claude Code still defaults
to built-in `Read`/`Grep`/`Glob` for many exploration steps where pgmcp tools
would produce better results (cross-project semantic queries, graph-aware
analysis, topic clustering). To bias Claude toward pgmcp's tools, pgmcp ships
three complementary mechanisms:

1. **Tool-call proxy via `PreToolUse` hooks** — augment or selectively deny
   `Read`/`Grep`/`Glob` calls at the harness level.
2. **Subagent containment via `~/.claude/agents/` overrides** — drop `Grep`/`Glob`
   from spawned-subagent tool catalogs entirely.
3. **Per-tool invocation counters** in `/api/status` — measure utilization to
   A/B-test whether the above are working.

The full design rationale (including why an HTTP-level proxy was rejected) lives
at `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`.
The user-side reference implementation lives in `~/.claude/hooks/` and
`~/.claude/agents/`.

### `PreToolUse` Hooks (Layer A: Augment + Layer B: Enforce)

Six hook scripts ship at `~/.claude/hooks/`, plus a shared library at
`~/.claude/hooks/lib/pgmcp-common.sh`. All are non-blocking: they exit 0
silently when the daemon is down (verified via 300 ms `GET /health`) so a
pgmcp outage never blocks the user.

**Layer A — augmenting hooks (always on, model-discretionary):**

| Hook                              | Matcher  | Behavior                                                                                                                                                               |
|-----------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `pgmcp-read-context.sh`           | `Read`   | Calls `POST /api/file_envelope` and injects a 5-line context block (language, size, indexed_at, etc.) alongside the file content.                                      |
| `pgmcp-grep-companion.sh`         | `Grep`   | When the path is broad (whole repo or no specific path), calls `POST /api/grep` and injects up to 10 cross-project hits alongside the native Grep result.             |
| `pgmcp-glob-suggestion.sh`        | `Glob`   | When the pattern is broad (`**/*.rs` from project root), emits a one-line suggestion to use `mcp__pgmcp__orient`/`semantic_search`/`project_tree` instead.            |

Augmenting hooks emit `additionalContext` and never block tool execution. They
are model-discretionary — the model decides whether to act on the injected
context.

**Layer B — enforce hooks (opt-in, harness-enforced):**

| Hook                              | Matcher  | Behavior                                                                                                                                                                     |
|-----------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `pgmcp-grep-enforce.sh`           | `Grep`   | When `PGMCP_HOOK_MODE=enforce` AND path is broad AND pattern length ≥ 3 chars, returns `permissionDecision: "deny"` and tells the model to use `mcp__pgmcp__grep` instead.   |
| `pgmcp-glob-enforce.sh`           | `Glob`   | When `PGMCP_HOOK_MODE=enforce` AND pattern is broad, returns `permissionDecision: "deny"` and tells the model to use `mcp__pgmcp__project_tree` or `mcp__pgmcp__orient`.    |

Enforce hooks use the same `permissionDecision: "deny"` primitive as
`~/.claude/git-guard.sh` — the harness refuses the tool call regardless of
model intent. There is **no** enforce hook for `Read` (too disruptive for
read-after-write and `.gitignore`'d files).

**Mode summary:**

| Mode (`PGMCP_HOOK_MODE`)  | Activation                  | What happens                                                                                                              |
|---------------------------|-----------------------------|---------------------------------------------------------------------------------------------------------------------------|
| `augment-only` (default)  | Always on                   | Layer A injects context; Layer B short-circuits. Soft nudging.                                                            |
| `enforce`                 | `PGMCP_HOOK_MODE=enforce …` | Layer B returns `permissionDecision: "deny"` for broad `Grep`/`Glob`. Native tool still allowed for narrow patterns.       |
| `permissive`              | `PGMCP_HOOK_MODE=permissive`| Same as `augment-only`; explicit override for sessions where enforce is configured per-project but the user wants out.    |

**Configuration** -- add to `~/.claude/settings.json` `PreToolUse` array
(alongside any existing `Bash`/etc. entries):

```json
{ "matcher": "Read",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-read-context.sh",   "timeout": 2000 }
]},
{ "matcher": "Grep",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-grep-companion.sh", "timeout": 3000 },
  { "type": "command", "command": "~/.claude/hooks/pgmcp-grep-enforce.sh",   "timeout": 1500 }
]},
{ "matcher": "Glob",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-glob-suggestion.sh","timeout": 1000 },
  { "type": "command", "command": "~/.claude/hooks/pgmcp-glob-enforce.sh",   "timeout": 1000 }
]}
```

The two `Grep` and two `Glob` matchers chain — both run for each tool call.
The enforce hook short-circuits unless `PGMCP_HOOK_MODE=enforce` and conditions
match, so the chain is harmless when enforce is off.

**Shared library** at `~/.claude/hooks/lib/pgmcp-common.sh` provides:

- `pgmcp_health_ok` — 300 ms `GET /health` probe; daemon down → fail-fast
- `pgmcp_emit_context` — shape `additionalContext` JSON for augmenting
- `pgmcp_emit_deny` — shape `permissionDecision: "deny"` JSON for enforce
- `pgmcp_dedup_check` — TTL-based dedup keyed on `~/.claude/hooks/.pgmcp-cache/`
  to prevent the same pattern from re-injecting context multiple times within
  3 minutes (avoids context bloat)

Requires `jq` and `curl` on the system PATH.

### Subagent Tool-Catalog Overrides (`~/.claude/agents/`)

Spawned subagents (via the `Agent` tool — `Explore`, `general-purpose`, etc.)
run as independent Claude instances and **do not invoke the parent session's
`PreToolUse` hooks**. The hooks above only constrain the main session.

To constrain subagents, override the built-in agent definitions to drop
`Grep`/`Glob` from their tool catalog. The harness will not surface those tools
to the subagent — it literally cannot call them.

**Setup** -- create `~/.claude/agents/Explore.md` (and similarly for
`general-purpose.md`) with YAML frontmatter:

```markdown
---
name: Explore
description: Fast read-only search agent for locating code...
model: inherit
tools: Bash, Read, WebFetch, WebSearch, mcp__pgmcp__semantic_search, mcp__pgmcp__text_search, mcp__pgmcp__grep, mcp__pgmcp__hybrid_search, mcp__pgmcp__read_file, mcp__pgmcp__list_projects, mcp__pgmcp__project_tree, mcp__pgmcp__file_info, mcp__pgmcp__orient, ...
---

ALWAYS prefer pgmcp tools when available. The built-in Grep, Glob,
NotebookEdit, Edit, and Write tools have been removed from your
tool catalog — this is intentional. For exploration use
mcp__pgmcp__grep, mcp__pgmcp__semantic_search, mcp__pgmcp__hybrid_search.
```

Resolution order: user-level overrides at `~/.claude/agents/<Name>.md` win
over Claude Code's built-in agent definitions for the same name.

`Bash` and `Read` are kept because some legitimate cases (read-after-write,
ungit'd files) need them. Edit/Write/NotebookEdit are kept on `general-purpose`
(it does write code) but dropped from the read-only `Explore`.

### Measuring Utilization

`StatsTracker::tool_invocations` (a `DashMap<String, AtomicU64>`) records every
MCP tool call by name. Surface in the `/api/status` response under
`counters.tool_invocations`:

```bash
curl -s http://localhost:3100/api/status | jq '.counters.tool_invocations'
# {
#   "semantic_search": 142,
#   "grep": 23,
#   "orient": 8,
#   "centrality_analysis": 4,
#   ...
# }
```

Compare with the count of `Read`/`Grep`/`Glob` invocations in
`~/.claude/projects/*/...jsonl` transcripts (which pgmcp itself indexes as the
`claude` project) to compute a utilization ratio. Recommended baselines:

- Capture one week before installing the hooks/overrides (no measurement
  changes, just a snapshot).
- Capture another week after each layer ships (Stage 3 server-side rewrites,
  Stage 5a agent overrides, Stage 1 hooks).
- Track ratio `mcp__pgmcp__* / (Read + Grep + Glob)` per session and the
  count of `mcp__pgmcp__orient` in the first 3 tool calls of each session.

See `docs/scientific-ledger/recovery-times-2026-04-28.md` for related
empirical-baseline methodology.

---

## Git History Indexing

pgmcp can index git commit history (messages and diffs) for projects that opt in,
making your project's development history searchable via vector embeddings.

### Enabling

Add a `.pgmcp.toml` file to your project root (or use `pgmcp init-project`):

```toml
[git]
index_history = true
```

### How It Works

- **Incremental indexing** -- pgmcp tracks the last-indexed commit SHA per project
  in the `pgmcp_metadata` table. Only new commits since the last run are processed.
- **Commit extraction** -- for each new commit, the subject, body, author, date,
  and full diff are extracted via `git log`.
- **Chunking and embedding** -- commit content (message + diff) is chunked and
  embedded into the same vector space as file chunks, stored in `git_commits` and
  `git_commit_chunks` tables.
- **Blame metadata** -- file chunks are annotated with `blame_commit`,
  `blame_author`, and `blame_date` columns, linking code to the commit that last
  touched it.
- **Co-change tracking** -- the `git_commit_files` table records which files
  changed in each commit, enabling co-change coupling analysis via
  `find_coupled_files`.
- **Cron job** -- the `git-history-index` job runs every hour by default
  (configurable via `git_history_index_interval_secs` in the `[cron]` section).

---

## Database Schema

### Core Tables

```
┌───────────────────┐       ┌────────────────────────┐       ┌──────────────────────┐
│     projects      │       │    indexed_files       │       │    file_chunks       │
├───────────────────┤       ├────────────────────────┤       ├──────────────────────┤
│ id     SERIAL     │──┐    │ id      BIGSERIAL      │──┐    │ id      BIGSERIAL    │
│ workspace_path    │  │    │ project_id INTEGER     │  │    │ file_id BIGINT       │
│ path   TEXT (UQ)  │  │    │ path    TEXT (UQ)      │  │    │ chunk_index INTEGER  │
│ name   TEXT       │  └───→│ relative_path TEXT     │  │    │ content TEXT         │
│ discovered_at TZ  │       │ language TEXT          │  └───→│ start_line INTEGER   │
│ last_scanned  TZ  │       │ size_bytes BIGINT      │       │ end_line INTEGER     │
└───────────────────┘       │ content TEXT           │       │ embedding vec(384)   │
                            │ content_hash BIGINT    │       │ blame_commit TEXT    │
                            │ line_count INTEGER     │       │ blame_author TEXT    │
                            │ truncated BOOLEAN      │       │ blame_date TZ        │
                            │ indexed_at TZ          │       └──────────────────────┘
                            │ modified_at TZ         │        UNIQUE(file_id, chunk_index)
                            └────────────────────────┘

┌───────────────────┐       ┌────────────────────────┐       ┌──────────────────────┐
│   git_commits     │       │  git_commit_chunks     │       │  pgmcp_metadata      │
├───────────────────┤       ├────────────────────────┤       ├──────────────────────┤
│ id    BIGSERIAL   │──┐    │ id      BIGSERIAL      │       │ key   TEXT (PK)      │
│ project_id INT    │  │    │ commit_id BIGINT       │       │ value TEXT           │
│ commit_hash TEXT  │  └───→│ chunk_index INTEGER    │       └──────────────────────┘
│ author TEXT       │       │ content TEXT           │
│ author_date TZ    │       │ embedding vec(384)     │
│ subject TEXT      │       └────────────────────────┘
│ body TEXT         │        UNIQUE(commit_id, chunk_index)
└───────────────────┘
 UNIQUE(project_id, commit_hash)
```

### Analysis Tables

| Table                        | Purpose                                            | Key Columns                                                                  |
|------------------------------|----------------------------------------------------|------------------------------------------------------------------------------|
| `cross_project_similarities` | Materialized chunk-pair similarity from batch scan | `chunk_id_a/b`, `chunk_similarity`, `project_name_a/b`                       |
| `code_topics`                | FCM topic clusters with c-TF-IDF labels            | `label`, `keywords`, `keyword_scores`, `chunk_count`, `file_count`           |
| `chunk_topic_assignments`    | Soft topic membership per chunk (fuzzy clustering) | `chunk_id`, `topic_id`, `membership_score`                                   |
| `git_commit_files`           | Files changed per commit (co-change coupling)      | `commit_id`, `file_path`, `change_type`                                      |
| `code_graph_edges`           | Import, co-change, and semantic edges              | `source_file_id`, `target_file_id`, `edge_type`, `weight`                    |
| `file_metrics`               | Precomputed per-file graph and quality metrics     | `pagerank`, `betweenness`, `instability`, `bug_proneness`, `tech_debt_score` |

### Indices

| Index                             | Type                             | Purpose                                   |
|-----------------------------------|----------------------------------|-------------------------------------------|
| `idx_chunks_embedding`            | HNSW (m=24, ef_construction=200) | Cosine similarity for semantic search     |
| `idx_git_commit_chunks_embedding` | HNSW (m=24, ef_construction=200) | Cosine similarity for git commit searches |
| `idx_files_fts`                   | GIN (tsvector)                   | Full-text search on file content          |
| `idx_files_path_trgm`             | GIN (pg_trgm)                    | Trigram similarity for path matching      |
| `idx_files_content_hash`          | B-tree                           | Fast skip-if-unchanged lookups            |
| `idx_files_project`               | B-tree                           | Filter files by project                   |
| `idx_files_language`              | B-tree                           | Filter files by language                  |
| `idx_git_commits_project`         | B-tree                           | Filter git commits by project             |
| `idx_cge_source`                  | B-tree                           | Graph edge source lookups                 |
| `idx_cge_target`                  | B-tree                           | Graph edge target lookups                 |
| `idx_cge_project_type`            | B-tree                           | Graph edges by project and type           |
| `idx_fm_project`                  | B-tree                           | File metrics by project                   |

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
exclude_patterns = ["node_modules", "target", ".git", "__pycache__", "*.lock"]

[[indexer.file_types]]
extension = "rs"
language = "rust"

[[indexer.file_types]]
extension = "py"
language = "python"

# ... 17 file types configured by default

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
| `database`   | `max_connections`                 | `20`                                | Connection pool size                           |
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
| `logging`    | `level`                           | `info`                              | Log level                                      |
| `logging`    | `rotation`                        | `daily`                             | Log rotation period                            |
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

## Usage

### CLI Commands

```bash
pgmcp init                       # Generate default config at ~/.config/pgmcp/config.toml
pgmcp upgrade-configs            # Upgrade global config + all indexed project configs
pgmcp upgrade-configs -i         # Same, but prompt before each project config
pgmcp serve                     # Run in foreground (stdout logging, stdio MCP transport)
pgmcp daemon                    # Run as daemon (file logging, HTTP MCP transport, sd-notify)
pgmcp stats                     # Print statistics from the database
pgmcp reindex                   # Clear the index and restart to re-index everything
pgmcp context                   # Print project context for current directory (for hooks)
pgmcp init-project              # Create .pgmcp.toml in current project directory
pgmcp upgrade-project           # Merge new .pgmcp.toml defaults into existing one
pgmcp analyze                   # Run all analysis jobs (similarity, topics, graph)
pgmcp analyze similarity        # Run only cross-project similarity scan
pgmcp analyze topics             # Run only topic clustering
pgmcp analyze graph              # Run only graph analysis
pgmcp results                   # Print cached analysis results (similarity + topics)
pgmcp results similarity        # Print similarity results only
pgmcp results topics             # Print topic results only
pgmcp tool                      # List all 41 MCP tools
pgmcp tool <name> [KEY=VALUE]   # Run any MCP tool from the command line
pgmcp tool <name> --schema      # Show tool's JSON Schema
pgmcp tool <name> --json [args] # Output compact JSON (for piping to jq)
```

Both `init-project` and `upgrade-project` accept `--cwd DIR` to specify the
project directory (defaults to `$PWD`).

#### `pgmcp context`

Prints a markdown summary of the project matching the current working directory,
including file count, language breakdown, and file tree. Designed to be called by
Claude Code hooks to inject project context automatically.

| Flag      | Default | Description                           |
|-----------|---------|---------------------------------------|
| `--cwd`   | `$PWD`  | Working directory to find project for |
| `--depth` | `3`     | Maximum depth for file tree           |

### Running as a Daemon

#### systemd Service

Create `/etc/systemd/system/pgmcp.service`:

```ini
[Unit]
Description=pgmcp - Software Engineering Intelligence Platform
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

---

## REST API

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

## MCP Capabilities

pgmcp advertises 5 of 8 MCP capabilities:

| Capability      | Description                                                                   |
|-----------------|-------------------------------------------------------------------------------|
| **Tools**       | 71 tools across 9 capability tiers                                            |
| **Resources**   | 2 static resources + 3 resource templates with URI parameters                 |
| **Completions** | Auto-completion for resource template parameters (`{name}`, `{path}`)         |
| **Logging**     | Server-to-client log push with dynamic verbosity control via `set_level()`    |
| **Tasks**       | Long-running async operations (reindex) with progress tracking & cancellation |

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

**Key exported metrics:**

| Metric                    | Type    | Description               |
|---------------------------|---------|---------------------------|
| `pgmcp_files_indexed`     | counter | Total files indexed       |
| `pgmcp_chunks_embedded`   | counter | Total chunks embedded     |
| `pgmcp_mcp_requests`      | counter | Total MCP requests served |
| `pgmcp_semantic_searches` | counter | Semantic search count     |
| `pgmcp_text_searches`     | counter | Text search count         |
| `pgmcp_grep_searches`     | counter | Grep search count         |
| `pgmcp_active_threads`    | gauge   | Active work pool threads  |
| `pgmcp_queue_depth`       | gauge   | Work pool queue depth     |
| `pgmcp_uptime_seconds`    | gauge   | Server uptime             |

Over 60 additional counters track analysis jobs, embedding operations, git indexing,
config reloads, and tool invocations. See `pgmcp tool index_stats` for the full set.

**Per-tool invocation map** -- `StatsTracker::tool_invocations` (a
`DashMap<String, AtomicU64>`) records every MCP tool call by name (e.g.
`semantic_search`, `grep`, `orient`). Surfaced under
`/api/status` → `counters.tool_invocations` rather than as individual
Prometheus metrics, since the key set is dynamic (new tools auto-register).
Used to A/B-test pgmcp utilization (see [pgmcp Utilization](#pgmcp-utilization-claude-code-integration) above).

### CLI Stats

```bash
pgmcp stats
```

Prints a summary of indexed files, projects, chunks, and bytes from the database.

---

<details>
<summary><strong>Implementation Details: Adaptive Thread Pool & Indexing Pipeline</strong></summary>

### Adaptive Thread Pool

The WorkPool dynamically scales its active worker count using a two-term objective
function minimized by a hill-climbing optimizer.

#### Exponential Moving Average (EMA)

Each metric is smoothed with an EMA to filter noise:

```
e_t = alpha * x_t + (1 - alpha) * e_{t-1}
```

where alpha = 0.15 (half-life ~ 4.3 samples at 200 ms intervals ~ 860 ms).

#### Objective Function

The scaling monitor minimizes:

```
J(N) = -w_tp * ema(throughput) + w_qd * ema(queue_depth)
```

| Weight | Default | Effect                             |
|--------|---------|------------------------------------|
| w_tp   | 1.0     | Reward higher throughput           |
| w_qd   | 2.0     | Penalize queue buildup (2x weight) |

Lower J(N) is better: high throughput with low queue depth.

#### Hill Climber

The optimizer uses +/-1 perturbation with geometric acceleration:

```
procedure SCALING_MONITOR:
    prev_completed <- pool.tasks_completed()
    loop every 200ms:
        throughput <- pool.tasks_completed() - prev_completed
        queue_depth <- pool.queue_depth()
        tp <- ema_throughput.update(throughput)
        qd <- ema_queue_depth.update(queue_depth)
        J <- -w_tp * tp + w_qd * qd

        improvement <- prev_J - J
        if improvement >= threshold:
            step_size <- min(step_size * 2, max_threads / 4)
            apply(direction, step_size)        // unpark or park
            cooldown <- 5 ticks
        elif improvement <= -threshold:
            direction <- -direction             // reverse
            step_size <- 1                      // reset acceleration
            apply(direction, 1)
        else:
            HOLD

        prev_J <- J
        prev_completed <- pool.tasks_completed()
```

Step size doubles on consecutive improvements in the same direction (geometric
acceleration), capped at `max_threads / 4`. On reversal, step size resets to 1.
A 5-tick cooldown (1 second) follows each scaling action to let the system
stabilize before re-measuring.

### Indexing Pipeline

The indexing pipeline is a reactive, lock-free chain of crossbeam channels:

```
FileEvent(path, kind)
  |
  +- Filter: is_configured_extension(path) AND NOT excluded(path)
  |
  +- Debounce: coalesce events by path within delta-t window
  |
  +- Dispatch: submit to WorkPool at priority in {HIGH, LOW}
  |
  +- process_file(path):
       1. content <- read(path)
       2. h <- xxHash3(content)
       3. if DB.content_hash[path] = h then SKIP
       4. file_id <- UPSERT indexed_files (content_hash = NULL)
       5. DELETE old chunks WHERE file_id = file_id
       6. chunks <- chunk(content, size=50, overlap=10)
       7. SEND chunks -> EmbeddingPool
       8. EmbeddingPool:
            embeddings <- model.embed(chunks)
            for (chunk, embedding) in zip(chunks, embeddings):
                INSERT file_chunks (chunk, embedding)
            UPDATE indexed_files SET content_hash = h   <- finalize
```

The two-phase commit ensures that a crash mid-indexing leaves `content_hash = NULL`,
which the integrity-check cron job detects and cleans up on the next cycle. No
partial state persists.

</details>

---

## Testing

```bash
# Unit tests + property-based tests (123 tests: 107 unit + 16 proptest)
cargo test --bin pgmcp

# Integration tests (requires Docker with PostgreSQL + pgvector)
cargo test --test integration -- --ignored

# MCP protocol tests (requires running PostgreSQL + built binary)
cargo test --test mcp_protocol -- --ignored
```

---

## Design Documents

Forward-looking architectural designs live under
[`docs/`](docs/), organized by purpose:

- [`docs/memory-server/`](docs/memory-server/) — full design for
  extending pgmcp into a SOTA memory server for LLM agents (entity/
  relation graph, bi-temporal facts, LLM-driven salience extraction,
  BGE-M3 embeddings, hierarchical & graph-enhanced retrieval,
  internal latent-space pipeline). Start with the directory
  [`README.md`](docs/memory-server/README.md) for an overview;
  decisions and rationale split across 11 files (00–10).
- [`docs/decisions/`](docs/decisions/) — short Architectural
  Decision Records (ADRs):
  - [`001-no-pgvectorscale-migration.md`](docs/decisions/001-no-pgvectorscale-migration.md)
    — why we stayed on pgvector HNSW.
  - [`002-sota-memory-server-design.md`](docs/decisions/002-sota-memory-server-design.md)
    — the 13 commitments behind the memory-server design.
- [`docs/scientific-ledger/`](docs/scientific-ledger/) — incident
  and debugging write-ups (OOM fix, recovery times).
- [`docs/DEVELOPING.md`](docs/DEVELOPING.md) — local development
  setup.

---

## License

Copyright 2026 Dylon Edwards

Licensed under the Apache License, Version 2.0. See [LICENSE.txt](LICENSE.txt) for
the full license text.
