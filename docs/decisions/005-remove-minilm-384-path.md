# ADR-005: Remove the MiniLM/384 embedding path (BGE-M3/1024-only)

- **Status:** Accepted
- **Date:** 2026-05-26
- **Completes:** ADR-004 (BGE-M3 embedding migration) — specifically its
  deferred "What this ADR does NOT do" items: dropping the legacy 384d
  `embedding` columns and removing the `MiniLm` backbone.
- **Supersedes:** the rollback-to-MiniLM affordances of ADR-004 (the
  `embed-cutover` CLI, the daemon downgrade guard, dual-dim dispatch).

## Context

ADR-004 shipped the BGE-M3 (1024d) migration as a *parallel-column* cutover
that deliberately retained the legacy MiniLM/384 path for one release as a
rollback path: the 384d `embedding` columns + their HNSW indices, the
`Backbone::MiniLm` BERT loader, the `pgmcp embed-cutover` CLI, the daemon
startup truth-table, and dim-dispatch that accepted both 384 and 1024.

That migration has soaked and the cutover is complete. Per project direction
the workspace now supports **only 1024-dim BGE-M3 embeddings** — *"there should
only be support for 1024-dim embeddings."* Retaining the dead 384 path is now
pure liability: dual-dim branching on every embedding read/write, a daemon
truth-table guarding against a downgrade that can no longer occur, ~1000 lines
of MiniLM backbone + cutover machinery, and 384-dim test fixtures that no longer
reflect reality.

## Decision

Remove the MiniLM/384 embedding path entirely. BGE-M3 (1024d, XLM-RoBERTa-Large)
is the only supported embedder.

### Schema
- DROP the legacy 384d `embedding` column **and** its HNSW index on the four
  dual-column tables — `file_chunks`, `session_prompts`, `git_commit_chunks`,
  `software_pattern_chunks` — idempotently (`DROP INDEX/COLUMN IF EXISTS` in
  `ensure_memory_v2_columns`). `embedding_v2 vector(1024)` is now the **sole**
  embedding column on these tables.
- Pin `pgmcp_metadata.active_embedding_signature` to `bge-m3-v1` (seed +
  `ON CONFLICT … DO UPDATE`).
- The 1024d-direct tables (`durable_mandates`, `session_mandates`,
  `memory_observations`, `memory_summary_tree`, `memory_entities`) are
  unchanged — they keep their canonical `embedding vector(1024)` column.
- The `embedding_v2` **name is retained for now**; a rename to `embedding` is a
  separate, mechanical follow-up.

### Embedder / signature (`src/embed/`)
- `EmbeddingSignature` collapses to a single variant `BgeM3V1`
  (`dim() == 1024`, `read_column() == "embedding_v2"`, `as_str() == "bge-m3-v1"`,
  `model_name() == "bge-m3"`). `resolve_signature_or_schema_default` always
  yields `BgeM3V1`.
- **Removed:** `ModelKind::MiniLm`, `Backbone::MiniLm` (the `BertModel` loader +
  `mean_pool_with_mask`), `MINILM_SIGNATURE`, `signature_for_model_name`, the
  `candle_transformers::models::bert` import. `ModelKind`/`Backbone` stay
  single-variant enums (closed, room to grow per the FCM-trait convention).
- Config defaults: `model = "bge-m3"`, `dimensions = 1024`.

### Dispatch (`src/db/`, `src/sessions.rs`)
- Every write/read dim-dispatch (`match embedding.len() { 384 => …, 1024 => … }`)
  collapses to 1024-only behind a `!= 1024` input-validation guard; the 384 arms
  (which wrote/read the now-dropped column and stamped `minilm-l6-v2`) are gone,
  along with the `map_legacy_embedding_insert_error` helper.
- The strict `!= 1024` guards in the memory tools are unchanged (always 1024).
- `topic_clustering` mmap/FCM buffers are sized at 1024 (were hardcoded 384).

### Cutover machinery removed (`src/cli/`, `src/cron/`)
- Deleted `pgmcp embed-cutover` (`src/cli/embed_cutover.rs`, the
  `Commands::EmbedCutover` variant + dispatch arm + module decl),
  `promote_to_bge_m3`, and the `active_embedding_signature` wrapper.
- The daemon startup signature truth-table (mid-migration / downgrade `bail!`
  arms) is replaced by a single **non-aborting** info log — no cross-signature
  state can exist post-cutover.
- `src/cron/embedding_migration.rs` remains as a forward-only 1024d
  `embedding_v2` **NULL-backfill** cron (for rows indexed before a model was
  configured); it no longer frames itself as a "manual cutover."

### Tests (`pgmcp-testing/`)
- Test infra flipped to 1024-d: `server_with_pool` + `DeterministicEmbeddingBackend`
  default + all `new(384)` call sites → `new(1024)`; `SyntheticCorpus::DIM`
  384→1024 with inserts switched to `embedding_v2` + `bge-m3-v1`; the
  dual-column migration-window tests rewritten to assert the 1024-only
  invariants; non-1024 dims retained **only** in negative tests that assert the
  guard rejects them.

## Consequences

- **No rollback to MiniLM.** Acceptable — ADR-004's migration soaked and the
  384 columns are dropped. Re-introducing MiniLM would be a fresh migration.
- **Simpler hot paths.** No dual-dim branching on insert/search; no daemon
  truth-table; ~1000 fewer lines of backbone + cutover code.
- **The migration cron stays** as a forward-only 1024 backfill (`WHERE
  embedding_v2 IS NULL`), so rows indexed before a model is configured still get
  embedded.
- **Regression guard.** The `no_legacy_chunk_embedding_sql` test fails the suite
  if any `src/` SQL ever again references the dropped chunk-`embedding` column
  (`c.embedding` / `ca.` / `cb.` / `c2.`); `migrations_post_cutover_idempotent`
  asserts `run_migrations` is a no-op against a post-drop schema.

## What this ADR does NOT do

- Does **not** rename `embedding_v2` → `embedding` (deferred; mechanical).
- Does **not** change HNSW params (still m=24, ef_construction=200,
  ef_search=100 per ADR-001) or the 1024d-direct tables.
- Does **not** touch the experiment subsystem's serde representation — that
  separate compile-time fix (adjacent tagging for the recursive
  `AcceptanceCriterion`) is **ADR-006**.
