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

