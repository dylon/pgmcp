# BGE-M3 embed-cutover Verification — Scientific Ledger

**Date opened:** 2026-05-25
**Host:** NVIDIA RTX 4060 Ti (8 GiB VRAM, Ada Lovelace, CC 8.9), Arch Linux
**ADR:** `docs/decisions/004-bge-m3-embedding-migration.md`
**Plan:** `~/.claude/plans/i-completed-pgmcp-embed-cutover-golden-thunder.md`
**Trigger:** operator ran `pgmcp embed-cutover --to bge-m3` then `--drop-legacy`
once the backlog drained to 0, and asked to confirm the migration succeeded and
that **all documents had their embeddings re-determined**.

---

## 1. Method (read-only)

```sh
./target/release/pgmcp embed-cutover --check --json     # canonical status
psql "postgres://pgmcp@localhost:5432/pgmcp" …          # ground-truth SQL:
#  - pg_attribute/format_type: which embedding columns + dims exist
#  - pg_indexes: which HNSW indexes exist
#  - per-table COUNT(*) of NULL embeddings + embedding_signature distribution
```

All verification was read-only (SELECT / EXPLAIN / `--check`); no rows mutated.

## 2. Result — dense cutover COMPLETE ✓

`--check`: `active_signature=bge-m3-v1`, `bundled=bge-m3-v1`,
`configured_model=bge-m3`, **backlog total = 0**, `safe_to_promote: true`.

Per-table coverage (every embedded row 1024-dim, stamped `bge-m3-v1`, zero
stale `minilm`, zero present-but-wrong-signature):

| Table | rows | NULL emb | `bge-m3-v1` | dim |
|-------|-----:|---------:|------------:|-----|
| file_chunks | 410,459 | 0 | 410,459 | vector(1024) |
| software_pattern_chunks | 3,193 | 0 | 3,193 | vector(1024) |
| session_prompts | 1,017 | 0 | 1,017 | vector(1024) |
| session_mandates | 592 | 0 | 592 | vector(1024) |
| git_commit_chunks | 252 | 0 | 252 | vector(1024) |
| durable_mandates | 0 | 0 | 0 | vector(1024) |

- Legacy 384d `embedding` columns **dropped** on all four dual-column tables;
  legacy HNSW indexes **dropped**; the four `*_embedding_v2` HNSW indexes present.
- Born-1024d tables (`memory_observations`, `memory_summary_tree`,
  `code_summary_tree`) are empty — never cutover targets. Nothing missed.
- A transient `session_mandates: 8` on the first `--check` cleared to 0 on
  re-check — post-cutover prompt-observe churn drained by the 10-min cron, not
  unmigrated data.

**Answer: yes — the dense BGE-M3 migration is complete; all documents were
re-determined.** Three secondary findings (below) were surfaced and acted on.

## 3. Findings

**F1 — contextual backfill stranded (latent bug).** `run_embedding_migration_pass`
(`src/cron/embedding_migration.rs`) short-circuited when
`full_backlog_counts().total() == 0`, but that count probes only dense
`embedding_v2`/`embedding` NULLs. The contextual re-embed (graph-roadmap Phase
2.4) and sparse backfill (2.3) run *after* that guard, so the instant the dense
backlog hit 0 the pass began returning early and `contextual_text` froze at
~4 % (16,384 / 410,459). It can never finish on its own.

**F2 — `sparse_v2` 100 % NULL (unwired, not corrupt).** `ensure_model_files`
(`src/embed/model.rs:645`) downloads only `pytorch_model.bin`/`config.json`/
`tokenizer.json`; BAAI/bge-m3 ships the sparse head as a *separate*
`sparse_linear.pt`, never fetched. The loader reads `vb.pp("sparse_linear").ok()`
from the backbone weights only → `has_sparse()==false` → the sparse step is
skipped every pass. Dense + BM25 `hybrid_search` never reads `sparse_v2`, so
nothing is broken — it is dormant infrastructure.

**F3 — `session_mandates` had no ANN index.** `ensure_memory_v2_hnsw_index`
(`src/db/migrations.rs`) built five v2 HNSW indexes but lacked a
`session_mandates.embedding` block (durable_mandates had one), even though the
column is populated. Copy-paste omission; no current read-path impact (the only
ANN path over session-mandate content is the `memory_unified_nodes` matview,
which has its own index).

## 4. Fixes applied

| # | Fix | File |
|---|-----|------|
| 1 | Short-circuit now skips only when **dense AND contextual** backlog are both 0; added `contextual_backlog_count()` mirroring `get_chunks_needing_context`'s selectable set (INNER `JOIN indexed_files`, `contextual_text IS NULL`, `embedding_v2 IS NOT NULL`). Sparse deliberately excluded (gated on `has_sparse()`, false here, else the pass would never short-circuit). | `src/cron/embedding_migration.rs` |
| 2 | Added the missing `session_mandates.embedding` HNSW block (`idx_session_mandates_embedding`, key `memory_v2_session_mandates_hnsw_params`), restoring symmetry with `durable_mandates`. | `src/db/migrations.rs` |

**F2 recommendation (not implemented — separately approvable):** wiring BGE-M3
sparse retrieval is a feature, not a fix. It needs (a) fetch
`sparse_linear.pt` (+ `colbert_linear.pt`) in `ensure_model_files`; (b) load a
second `VarBuilder::from_pth` and build the sparse `Linear` from it; (c) a read
path that consumes `sparse_v2` (today nothing does) and an extension of the §4.1
guard to count sparse backlog when `has_sparse()`. Recorded for a future
decision.

## 5. Operational completion — contextual drain (operator Q1 = "let it finish")

After fixes 1–2 build green and the release binary is rebuilt, the operator
restarts the daemon (operator-owned). The fixed cron then drains `contextual_text`
to 0. To finish quickly the throttle is temporarily raised
(`[cron] embedding_migration_interval_secs` ↓, `embedding_migration_max_batches`
↑); at default 2048 rows/600 s the ~394k remaining rows take ~32 h, GPU-bound
~1–3 h with the bump. Contextual re-embed **overwrites `embedding_v2`** with
prefix‖content, so on completion all file_chunks carry uniform contextualized
1024d vectors. Then `embedding_migration_interval_secs = 0` + restart turns the
cron off.

Drain progress query:
```sql
SELECT COUNT(*) FROM file_chunks c JOIN indexed_files f ON f.id=c.file_id
WHERE c.contextual_text IS NULL AND c.embedding_v2 IS NOT NULL;   -- target: 0
```

**As-built status (2026-05-26, COMPLETE):** Fixes 1–2 landed; nextest-green
(1162/1162); my files fmt-clean; release binary rebuilt (00:20). The operator
folded the drain into their concurrent A2A/RLM session's unified rebuild+restart
(daemon up ~11:17). Daemon reached Ready ~11:30; the migration cron then drained
`contextual_text` from 382,446 to **0 at 17:47** (~6.3h, ~17 rows/sec,
GPU-bound, no stall) — coverage **414,745 / 414,772 = 99.99%** (the remainder is
steady-state churn from files indexed after 0, swept by the still-running cron).
Drain config then reverted: `pool_size 1→2`, `embedding_migration_interval_secs
120→0` (cron disabled per Q1 "then stop"), `max_batches` override removed —
effective on the next daemon restart. Dense cutover stayed clean throughout
(`active=bge-m3-v1`, 0 NULL dense, `safe_to_promote: true`). Final whole-tree
`verify.sh` is gated on the concurrent A2A session compiling (it was mid-refactor
— `E0583` `mod acceptance` / earlier `E0308`); my three files pass
fmt/build/clippy/tests + nextest, and the A2A session's own verify.sh run covers
them.

## 6. Verification

- Static: `./scripts/verify.sh` (fmt, build/clippy `--all-targets -D warnings`,
  test suites, GPU smoke) green.
- Schema (fix 2): after restart, `pg_indexes` for `session_mandates` includes
  `idx_session_mandates_embedding`.
- Behaviour (fix 1): daemon logs show `file_chunks_contextualized` climbing per
  tick (was 0/stranded); the drain query above reaches 0.
- No regression: `embed-cutover --check` still reports backlog 0,
  `active_signature=bge-m3-v1`, `safe_to_promote: true`.
