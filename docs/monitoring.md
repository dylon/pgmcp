# pgmcp Monitoring

Prometheus metrics, the adaptive thread-pool tuner, and the indexing
pipeline observability surface. For daemon operations see
[operations.md](operations.md).


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

