# pgmcp Architecture

Implementation-level architecture for pgmcp. For tool capabilities see
[tool-catalog.md](tool-catalog.md); for daemon operations see
[operations.md](operations.md).


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

