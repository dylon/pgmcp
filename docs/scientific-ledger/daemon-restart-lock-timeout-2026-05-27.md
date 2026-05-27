# Daemon-restart lock-timeout outage + cold-start latency — 2026-05-27

## 1. Symptom

Restarting the daemon aborted startup:

```
slow statement … "ALTER TABLE indexed_files ALTER COLUMN content_hash DROP NOT NULL" elapsed 5.000320748s
Error: error returned from database: canceling statement due to lock timeout
Caused by: canceling statement due to lock timeout
```

The restart landed while the heavy `semantic-edges` cron (6-hour interval) was
mid-scan. The new instance could not boot.

## 2. Root-cause analysis

A lock collision against an **orphaned PostgreSQL backend** left by the previous
instance, made fatal by an **unguarded, lock-escalating startup migration**.

```
  t₀≈14:55:18  old daemon: semantic-edges runs compute_semantic_file_edges for
               project "codex" — a CROSS JOIN LATERAL HNSW scan inside a txn
               (SET LOCAL statement_timeout='5min'); holds ACCESS SHARE on
               indexed_files + file_chunks.
       ┊
  14:59:42.6   restart → old daemon async_main returns, main() drops the tokio
               runtime out from under the in-flight query. Client socket closes;
               the query future dies ("A Tokio 1.x context … is being shutdown").
       ┊       ┌─ client_connection_check_interval = 0 (PG default, never set) ─┐
       ┊       │ PostgreSQL does NOT notice the dead client during a long query │
       ┊       │ → the backend keeps running server-side, still holding         │
       ┊       │   ACCESS SHARE, until its 5-min statement_timeout (~15:00:18). │
       ┊       └───────────────────────────────────────────────────────────────┘
  14:59:54.9   NEW daemon boots; run_migrations re-runs the inline schema block.
  14:59:54.9   first ALTER TABLE indexed_files (content_hash DROP NOT NULL) needs
               ACCESS EXCLUSIVE → blocks on the orphan's ACCESS SHARE.
  14:59:59.9   lock_timeout (5s) fires → SQLSTATE 55P03 → run_migrations(…).await?
               propagates → startup aborts.
```

Key correction to the obvious-but-wrong fix: guarding *only* `content_hash DROP
NOT NULL` is insufficient — in PostgreSQL the `ALTER TABLE` lock level is chosen
from the command type *before* the `IF NOT EXISTS` no-op check, so the next
`ALTER TABLE indexed_files ADD COLUMN IF NOT EXISTS …` statements **also** take
ACCESS EXCLUSIVE and would collide identically. The real fix must make the
orphaned lock disappear and make startup tolerant of transient contention.

## 3. Hypotheses → fixes (all verified to compile; tests below)

| # | Hypothesis | Fix | Files |
|---|------------|-----|-------|
| H1 | A dead client's backend self-terminates within `client_connection_check_interval` of disconnect, freeing locks before the next boot's migrations run. | Set `client_connection_check_interval` (default 10s, PG≥14, version-gated) + base `application_name='pgmcp'` in `after_connect`; add `client_connection_check_interval_ms` knob. | `src/db/pool.rs`, `src/config.rs` |
| H2 | Retrying `run_migrations` on 55P03 rides out still-draining contention instead of aborting. | `run_migrations_with_lock_retry` (6×, 5s backoff, 55P03 only); daemon calls it. | `src/db/migrations.rs`, `src/cli/daemon.rs` |
| H3 | Guarding the lock-escalating DDL removes a needless ACCESS EXCLUSIVE per boot and honors the block's "idempotent" claim. | `column_is_nullable` guard on `content_hash DROP NOT NULL`; `ensure_named_constraint` (COMMENT-stamped) for the 4 CHECK re-installs that previously revalidated whole tables every boot. | `src/db/migrations/schema_introspect.rs`, `src/db/migrations.rs` |
| H4 | Reaping labeled heavy backends on graceful shutdown closes the orphan window to ~0. | `SET LOCAL application_name='pgmcp:heavy:<job>'` on the 5 heavy cron txns; `terminate_heavy_backends` sweep run early in shutdown (before work-pool drain, before pool close). | `src/db/admin.rs`, `src/db/queries.rs`, `src/cron/graph_analysis.rs`, `src/cli/daemon.rs` |

**A and D are complementary:** D handles graceful restart (the daemon runs the
sweep); A is the safety net for ungraceful death (SIGKILL/OOM/crash) where no
sweep can run.

## 4. Verification

### 4.1 Compile
`cargo check --workspace` → exit 0, zero warnings in the `pgmcp` crate (all
warnings are in dependency crates). _(2026-05-27)_

### 4.2 Regression tests
`pgmcp-testing/tests/daemon_restart_lock_safety.rs` (`require_test_db!`):
- `migrations_guard_content_hash_drop_not_null` (H3): force `content_hash NOT
  NULL`, re-run migrations → Ok, column nullable, idempotent second run.
- `terminate_heavy_backends_targets_only_labeled` (H4): a `pgmcp:heavy:test`
  holder is terminated; an unlabeled control holder is not.
- `migrations_retry_through_lock_contention` (H2): with `lock_timeout=1s` and a
  held ACCESS EXCLUSIVE, the first attempt 55P03s and the retry succeeds once the
  lock releases.

Result: _PENDING — populated when run against a test DB (the CREATEDB-gated
harness self-skips in the local dev environment)._

### 4.3 Live validation (after rebuild + restart)
- `SHOW server_version_num;` (expect ≥ 140000) and `SHOW
  client_connection_check_interval;` → `0` before, `10s` on a new connection
  after. _PENDING._
- Orphan reproduction: `psql` `BEGIN; SELECT 1 FROM indexed_files; ` then
  `kill -9` the psql; watch `pg_stat_activity`/`pg_locks` — backend vanishes
  within the interval (vs. lingers when 0). _PENDING._

## 5. Part 2 — cold-start latency (expose `/health` + accept requests ASAP)

### 5.1 Finding (corrects an initial assumption)
The embedding-model load is **already** off the critical path: `EmbeddingPool::new`
spawns worker threads and returns immediately; each worker loads BGE-M3 inside its
own thread (`src/embed/pool.rs::embedding_worker`). The only blocking pre-bind
costs were (a) migrations (Part 1 keeps these fast/robust) and (b) the **optional**
reranker (`[api] rerank_hook`) and LLM-extractor (`[memory.extractor]`) model
loads. `/health` was also gated on the *initial scan completing* and bound dead
last, so during startup a probe got connection-refused, then 503 for the whole scan.

### 5.2 Changes (verified to compile, `cargo check` clean)
- **Serving-ready `/health`** (`src/api/handlers.rs`): 200 once DB pool is up **and**
  ≥1 embedder worker is loaded — decoupled from the scan-complete `Ready` phase, so
  RAG/search work *during* the initial scan. Body reports `phase` + readiness flags.
- **Embedder readiness** (`src/embed/pool.rs`, `src/embed/mod.rs`):
  `QueryEmbedder::is_ready()` / `EmbedSource::is_ready()` read the existing
  `embed_workers_alive` counter.
- **Request gating**: `/api/search` returns a fast 503, and the `semantic_search` /
  `hybrid_search` MCP tools a clear retryable error, when no worker is ready —
  instead of parking in the bounded query channel past the RAG hook's budget.
- **Background model loads + hot-swap**: the reranker (`ApiState.reranker`) and LLM
  extractor (`SystemContext` + `ApiState`) now load in `spawn_blocking` and
  hot-swap into `Arc<parking_lot::RwLock<Option<Arc<dyn …>>>>` cells, so neither
  blocks the listener bind. (`arc_swap::ArcSwapOption` can't hold a `dyn` trait
  object — its `RefCnt` requires `Sized` — hence `RwLock`; the snapshot is taken
  into a local so no guard is held across an `await`, keeping handlers `Send`.)
- **Parallel FS walk** (`src/indexer/scanner.rs`): `scan_single_workspace` uses the
  `ignore` crate's `build_parallel()`. *Data-driven caveat:* the walk is background
  and not the bottleneck — embedding throughput (`embeddings.pool_size` × GPU batch)
  is — so this trims traversal only and does not change time-to-serving-ready.
- **`recovery_times` markers**: `phase="listening"` at bind (`src/cli/daemon.rs`)
  and `phase="scan_complete"` at scan end (`src/indexer/event_processor.rs`); the
  embed-warmup `phase="ready"` marker already existed.

### 5.3 Decisions
Q1 → serving-ready `/health`; Q2 → bind after migrations (not before). Heavy-cron
gating still keys off the scan-complete `Ready` phase (unchanged).

### 5.4 Verification
_PENDING live measurement on rebuild/restart: compare `recovery_times`
`listening` / `ready` / `scan_complete` timestamps before vs. after to confirm
time-to-serving-ready ≪ time-to-scan-complete, and `/health` reachable (503→200)
within ~1–2 s on a warm DB._
