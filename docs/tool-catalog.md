# pgmcp Tool Catalog

This is the complete catalogue of pgmcp's MCP tools. Each tier is independent
— you can run pgmcp with any subset of tiers enabled, and the daemon dispatches
to them on demand. See [the README](../README.md) for installation and
quick-start instructions.

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
- **50+ file types** -- Rust, Python, TypeScript, JavaScript, Java, Scala, C/C++, Clojure/ClojureScript, Rholang, MeTTa, Coq, TLA+, Lean, Sage, Prolog, Shell, JSONL, Markdown, and more
- **17 tree-sitter symbol-extraction backends** -- Rust (via `syn`), Python, JavaScript, TypeScript (incl. TSX), Java, Scala, C, C++, Rholang (full process-calculus coverage incl. let bindings + local channels + per-contract complexity metrics), MeTTa (head-driven S-expression dispatch: `=`/`:=` rule defs, `:` type annotations, `import!` modules), Clojure, ClojureScript, Coq, TLA+, Lean, Sage
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

