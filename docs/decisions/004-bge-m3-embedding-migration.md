# ADR-004: BGE-M3 embedding migration (full cutover)

- **Status:** Accepted
- **Date:** 2026-05-23
- **Supersedes:** the Phase 5 section of
  `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
  (originally ModernBERT-base 768-dim)

## Context

The memory-server Phase 1 milestone (`7385761`) added parallel-column
schema, a BGE-M3 model loader, and a background-fill cron — but only on
two tables (`file_chunks`, `session_prompts`). Four other
embedding-bearing tables were left out of the cron's coverage:
`git_commit_chunks`, `software_pattern_chunks`, `durable_mandates`,
`session_mandates`. The mandate tables were created with the target
1024d shape directly but never had a writer.

A production codex hit the cost of the partial state:

```
pgmcp/memory_raptor_search
  Mcp error: -32603: query failed: expected 1024d embedding, got 384
```

`memory_raptor_search` (and every other memory-graph tool) hardcoded a
strict-1024d guard. Daemons configured for MiniLM produced 384d
queries; the guard rejected them with a user-hostile error.

This ADR closes the gap. The full migration completes the Phase 1
parallel-columns pattern across every embedding-bearing table,
hardens the daemon against mis-aligned (model, active_signature)
configurations, and ships an operator CLI for driving the cutover.

## Decision

Adopt BGE-M3 (XLM-RoBERTa-Large, 1024d, multilingual) as the
canonical embedder, replacing `all-MiniLM-L6-v2` (384d, English-only).
Migrate every embedding column via parallel columns + signature
gating + manual cutover.

## Why BGE-M3 (and not ModernBERT-base)

1. **Multilingual.** 100+ languages. pgmcp indexes Rust, Python,
   Scala, Rholang, MeTTa, Lean, Coq, TLA+, JavaScript, TypeScript,
   Clojure, Java, C, C++ source, plus prose-comment / Markdown
   content that frequently mixes languages. ModernBERT-base is
   English-only.
2. **Multi-functionality.** BGE-M3 produces dense + sparse +
   multi-vector representations in a single forward pass. Phase 8
   reranker work gets the sparse output for free.
3. **Better MTEB / MIRACL retrieval scores** at the same VRAM budget
   (~1.2 GiB fp16 vs ~0.85 GiB ModernBERT-base; the recall delta is
   larger than the cost delta).
4. **Matryoshka-truncatable** to 64/128/256/512/1024 — enables cheap
   ANN-then-rerank without re-embed.
5. **Working candle XLM-RoBERTa loader** today; ModernBERT loader
   was upstream-pending at the time of the original Phase 1
   decision.
6. The original integration plan's Phase 5 selected ModernBERT
   before the memory-server design (ADR-002) committed to BGE-M3.
   This ADR ratifies the BGE-M3 choice as already-shipped and
   extended.

## Why parallel columns (and not single-column rename)

1. **Atomic cutover, no downtime.** Flipping
   `pgmcp_metadata.active_embedding_signature` is one Postgres
   `UPDATE`; readers consult it via a 30 s-TTL in-process cache
   (`src/embed/signature.rs::ActiveSignatureCache`).
2. **Rollback path.** As long as the legacy `embedding` column hasn't
   been dropped, flipping the signature back to `minilm-l6-v2`
   restores legacy reads. `pgmcp embed-cutover --to minilm --force`
   is the explicit rollback CLI.
3. **No wrong-dim window.** A single-column approach (drop, add
   1024d, backfill, drop placeholder) would leave queries during the
   backfill returning empty or dim-mismatched results.

## Tables migrated

| Table                       | Legacy column         | New column           | Migration writer (C5)               |
|-----------------------------|-----------------------|----------------------|--------------------------------------|
| `file_chunks`               | `embedding (384)`     | `embedding_v2 (1024)`| `migrate_file_chunks_batch`          |
| `session_prompts`           | `embedding (384)`     | `embedding_v2 (1024)`| `migrate_session_prompts_batch`      |
| `git_commit_chunks`         | `embedding (384)`     | `embedding_v2 (1024)`| `migrate_git_commit_chunks_batch`    |
| `software_pattern_chunks`   | `embedding (384)`     | `embedding_v2 (1024)`| `migrate_software_pattern_chunks_batch` |
| `durable_mandates`          | (none; 1024d-direct)  | `embedding (1024)`   | `migrate_durable_mandates_batch`     |
| `session_mandates`          | (none; 1024d-direct)  | `embedding (1024)`   | `migrate_session_mandates_batch`     |

`memory_observations`, `memory_summary_tree`, and `memory_entities`
were already 1024d-direct (Phase 2 memory-server schema); they're
written by the memory-raptor cron and stay unchanged.

`code_topics.centroid` is `REAL[]` — dim-agnostic; topic clustering
follows the source-chunk dim automatically.

`cross_project_similarities` stores chunk-id pairs + similarity
scores; no embedding column.

## Mid-cutover correctness invariants

1. **Indexer-active-signature lock-step** (C4). Daemon startup
   refuses two combinations: `[embeddings].model = "bge-m3"` with
   `active_signature = "minilm-l6-v2"` AND migration cron disabled
   (would write 1024d into nowhere); `model = "all-MiniLM-L6-v2"` with
   `active_signature = "bge-m3-v1"` (downgrade — would corrupt recall).
2. **Dim-aware writes** (C3). Every insert helper switches on
   `embedding.len()` → routes to the right column + stamps the right
   signature.
3. **Dim-aware reads** (C6/C7/C8). Every MCP tool that reads an
   embedding column either dispatches on the query embedding's dim
   (`recall_prompts_semantic`, `semantic_search`, `hybrid_search`,
   `semantic_search_commits`, `semantic_search_patterns`,
   `tool_anomaly_detection`, `tool_doc_code_drift`,
   `tool_semantic_drift`, `tool_embedding_outliers`,
   `tool_lsh_clone_detection`, `tool_adoption_lag`) or resolves
   `active_embedding_signature` (bulk-extract centroid reads, no
   query vector).
4. **Cutover atomicity** (C9). `promote_to_bge_m3(false)` refuses
   when any of the 6 tables has `embedding_v2 IS NULL` rows; passes
   only when `migration_complete() == true`. `pgmcp embed-cutover
   --check` shows the per-table backlog.
5. **Cron skip-stamps** (C5). The cron's `WHERE embedding_v2 IS NULL`
   skips rows already populated by post-Phase-5 indexer dual-writes,
   so it only processes legacy pre-Phase-5 backlog.
6. **Drop is opt-in** (C9). `pgmcp embed-cutover --drop-legacy` drops
   the 384d columns + HNSW indices only after `active_signature` is
   `bge-m3-v1` and the operator has soaked for as long as they want.

## What this ADR does NOT do

- Does not drop the legacy `embedding` column automatically.
  Deferred to operator-driven `pgmcp embed-cutover --drop-legacy`.
- Does not change HNSW params (still m=24, ef_construction=200,
  ef_search=100 per ADR-001).
- Does not migrate `code_topics.centroid` (already untyped `REAL[]`,
  dim-agnostic).
- Does not remove the `MiniLm` arm of `Embedder::Backbone` from the
  codebase. Stays for one release as a rollback path; gets removed
  in a later release once operators have soaked through cutover.
- Does not change the REST API in `src/api/` (no embedding endpoints
  there today).

## Commit sequence (Phase 5 in `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`)

| Commit | Subject                                                  |
|--------|----------------------------------------------------------|
| C0     | register embedding-migration cron in scheduler           |
| C1     | extend `_v2` schema to git_commit_chunks + sp_chunks     |
| C2     | `src/embed/signature.rs` — active-signature cache        |
| C3     | dim-aware dispatch in all 5 insert paths                 |
| C4     | daemon startup truth-table refusal                       |
| C5     | extend embedding_migration cron to all 6 tables          |
| C6     | document cache-vs-len dispatch equivalence               |
| C7     | signature-aware reads in 6 inline-SQL tools              |
| C8     | signature-aware reads in 5 dispatch queries              |
| C9     | `pgmcp embed-cutover` CLI subcommand                     |
| C10    | extend memory_unified_nodes matview                      |
| C11    | this ADR + tests + docs                                  |

## Operator runbook

```bash
# 1. Pre-flight — confirm current state.
pgmcp embed-cutover --check

# 2. Switch the daemon's configured model.
$EDITOR ~/.config/pgmcp/config.toml
# [embeddings]
# model = "bge-m3"
# dimensions = 1024
# [cron]
# embedding_migration_interval_secs = 600   # 10 min cron tick

# 3. Restart the daemon. It will start in mid-migration mode
#    (writes 1024d for new chunks, cron drains legacy backlog).
systemctl --user restart pgmcp

# 4. Watch backlog drain (re-run periodically).
pgmcp embed-cutover --check

# 5. When backlog hits zero, flip the active signature.
pgmcp embed-cutover --to bge-m3

# 6. Soak through normal usage.

# 7. (Later) drop the legacy 384d columns.
pgmcp embed-cutover --drop-legacy
```
