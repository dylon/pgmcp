# 09 — Milestones & as-built log

Coarse-grained milestones for tracking. Each milestone bundles one
or more phases, ends with `scripts/verify.sh` green and the matching
integration tests landed.

The **"Shipped" log** at the bottom is appended-to as each milestone
lands, with date, commit hash range, deviations from the plan, and a
link to the eval-harness scorecard. Per the discipline in
[`README.md`](README.md), every implementation PR updates this file.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §18 +
> §20.5.

---

## Milestones (forward-looking)

### M1 — Foundations

**Phases:** Phase 0 (quick wins) + Phase 1 (BGE-M3 embedding upgrade)
+ **initial seed of `docs/memory-server/`** (this directory).

**Done when:**

- `recall_prompts`, `search_mandates`, mandate-supersession all
  shipped behind their MCP names.
- BGE-M3 embedded everywhere; cutover flag flipped to
  `bge-m3-v1`.
- `docs/memory-server/` is in the repo, linked from the project
  `README.md`, with the ADR
  `docs/decisions/002-sota-memory-server-design.md` pointing here.

### M2 — Memory graph live

**Phases:** Phase 2 (schema) + Phase 3 (official-compat subset of
CRUD tools).

**Done when:**

- All Phase 2 tables created with bi-temporal columns and partial
  indices.
- The 9 official `@modelcontextprotocol/server-memory` tools are
  registered and shape-compatible.
- Agents that target the official server can swap pgmcp in
  unchanged.

### M3 — Memory graph SOTA-shaped

**Phases:** Phase 3 extensions (semantic_search, hybrid_search,
facts_at, anchors).

**Done when:**

- Bi-temporal point-in-time queries work end-to-end via
  `memory_facts_at`.
- Code-crosslink queryable both ways
  (`memory_find_code_for_entity`, `memory_find_entities_for_code`).

### M4 — Intelligent writes

**Phases:** Phase 4 (LLM extractor) + Phase 5 (reflection).

**Done when:**

- Qwen3-8B Q4 loads under the 8 GB ceiling and extracts schema-valid
  JSON.
- `POST /api/session/observe` runs Stage A (existing regex) + Stage
  B (LLM extraction) end-to-end.
- `memory_reflect` MCP tool emits at least one higher-order
  observation with `derived_from` populated.

### M5 — SOTA retrieval

**Phases:** Phase 6 (RAPTOR / PPR / NodeRAG heterogeneous-node view
/ PathRAG paths / empirical gating) + Phase 7 (reranker).

**Done when:**

- All Phase 6 tools wired in: `memory_raptor_search`,
  `memory_ppr_search`, `memory_unified_search`, `memory_neighbors`,
  `memory_path_search`.
- BGE-reranker-v2-m3 loaded as the cross-encoder reranker; opt-in
  via `rerank=true`.
- Phase 6.5 empirical-gating discipline in place: A/B vs vanilla in
  Phase 9 eval, auto-disable on regression, Prometheus counters
  surfaced.

### M6 — Production-ready

**Phases:** Phase 8 (eviction) + Phase 9 (internal eval) + Phase 10
(client profiles).

**Done when:**

- `memory_forget` works for both soft-delete (default) and
  cascade=true with full audit manifest.
- `memory-retention` cron hard-deletes rows past their window
  without breaking superseded chains.
- The internal eval harness covers ≥ 20 scenarios across recall,
  contradiction, multi-hop, cross-graph, scope isolation, tier
  filtering, forgetting, reflection.
- Per-client OutputFormat resolves correctly for Claude Code, Codex
  CLI, and generic clients.

### M7 — Latent-pipeline opt-in

**Phases:** Phase 11 (RecursiveMAS-style internal latent pipeline).

**Done when:**

- RecursiveLink trained against the user's Qwen3-8B; weights
  versioned under `link_signature`.
- Latent pipeline behind a config flag (`[memory.latent_pipeline]
  enabled = true`).
- Daily quality validation harness running and writing
  `(latent_score, text_score, delta)` to `pgmcp_metadata`.
- Token/latency telemetry surfaced via
  `/api/memory/pipeline_stats`.
- Targets per the cross-phase concerns in
  [`02-phases.md`](02-phases.md) met (≥ 30% token reduction, ≥ 1.5×
  speedup on extract→reflect, no quality regression).

---

## Shipped log (append-only)

> Convention: one entry per landed milestone (or sub-phase if a
> milestone ships in pieces). Include date, commit range, deviations
> from plan, eval results.

### M1 — Foundations

- **Status:** in progress.
- **Sub-step 1 (docs seeded):** ✅ shipped (commit `9bf5624`) —
  initial commit of `docs/memory-server/` and
  `docs/decisions/002-sota-memory-server-design.md`, with the repo
  `README.md` linking to the new design directory.
- **Sub-step 2 (Phase 0 quick wins):** ✅ shipped —
  - **`recall_prompts`** MCP tool: vector search over the
    `session_prompts.embedding` column that had zero readers before
    this. Filters by `project` and `session`; returns top-k by
    cosine similarity. Wired through `RecallPromptsParams` →
    `tool_recall_prompts` → `queries::recall_prompts_semantic`.
  - **`search_mandates`** MCP tool: Postgres FTS over
    `durable_mandates.imperative || target` with optional
    `polarity`, `scope`, and `project_id` filters. The same tool
    will gain a semantic mode after Phase 1 cutover adds a 1024d
    embedding column.
  - **Mandate supersession**: `mark_near_duplicate_superseded` marks
    active session mandates with `lower(imperative)` Levenshtein
    ≤ 3 (same session, same polarity) as `'superseded'`. Wired into
    `POST /api/session/observe` after `upsert_mandate` runs.
    Requires `CREATE EXTENSION fuzzystrmatch` (added to migrations).
    Counter: `pgmcp_memory_mandate_supersessions`.
  - **Telemetry**: three new Prometheus counters
    (`pgmcp_memory_recall_prompts`,
    `pgmcp_memory_search_mandates`,
    `pgmcp_memory_mandate_supersessions`) and matching
    `AtomicU64` fields on `StatsTracker`.
  - **Tests**: 8 new integration tests in
    `pgmcp-testing/tests/memory_phase0.rs` covering top-k vector
    search, session filtering, FTS match, polarity filter,
    invalid-polarity rejection, edit-distance dedupe (positive,
    negative, and polarity-isolation cases). All pass.
  - **Verification gate**: `./scripts/verify.sh` green.
- **Sub-step 3 (Phase 1 embedding upgrade):** ✅ shipped —
  - **Schema**: `file_chunks` and `session_prompts` gained
    `embedding_v2 vector(1024)` + `embedding_signature TEXT`
    (idempotent `ADD COLUMN IF NOT EXISTS`). HNSW indices on the
    new columns. Same shape added to `durable_mandates` and
    `session_mandates` to unblock Phase 0 Section 3.3 promotion.
  - **`pgmcp_metadata.active_embedding_signature`** row seeded
    with `'minilm-l6-v2'`; flipped to `'bge-m3-v1'` after the
    operator drains the backlog.
  - **BGE-M3 embedder** (`src/embed/model.rs`): refactored
    `Embedder` into a closed-set backbone enum
    (`MiniLm(BertModel)` vs `Bgem3(XLMRobertaModel)`); each
    backbone owns its pooling strategy (mean-pool with mask vs
    CLS-pool) and produces L2-normalized vectors of its dim
    (384 / 1024). HF cache resolves
    `BAAI/bge-m3` on first use.
  - **Matryoshka helper** `matryoshka_truncate` (prefix-truncate +
    re-normalize) ready for Phase 6 / 7 query-time downsampling.
  - **Embedding-migration cron** (`src/cron/embedding_migration.rs`):
    drains `embedding_v2 IS NULL` rows from both tables in
    `batch_size × max_batches` chunks per tick (default 64 × 32 =
    2048 rows). Uses `FOR UPDATE SKIP LOCKED` so concurrent ticks
    don't race. Operator helpers `migration_complete`,
    `promote_to_bge_m3(force=false)`, and
    `active_embedding_signature` round out the cutover surface.
  - **Cutover dispatch** in `recall_prompts_semantic`: routes 384d
    queries to `embedding` and 1024d queries to `embedding_v2`;
    rejects other dims with a clear protocol error so a misconfig
    can't silently produce wrong-shape arithmetic.
  - **Telemetry**: four new counters
    (`embeddings_migration_runs`, `embeddings_migrated_file_chunks`,
    `embeddings_migrated_session_prompts`,
    `embeddings_migration_errors`) wired through the JSON snapshot
    and the Prometheus exposition.
  - **Tests**: 5 active + 1 ignored integration tests in
    `pgmcp-testing/tests/memory_phase1.rs` cover the new column,
    operator-helper semantics, cutover dispatch, and dim
    rejection. The `#[ignore]`-gated test downloads the BGE-M3
    weights and validates 1024d L2-normalized output — opt-in to
    avoid a 1.2 GB pull on every `cargo test`.
  - **Verification gate**: `./scripts/verify.sh` green (604 unit
    tests, format check, clippy zero-warnings, full integration
    suite).
- **M1 status:** ✅ all sub-steps shipped.

### M2 — Memory graph live

- **Status:** ✅ shipped.
- **Phase 2 schema:** `memory_tier` and `memory_source` ENUMs;
  `memory_scope` (bi-temporal scope tuple with optional
  user/agent/session/project dimensions); `memory_entities`,
  `memory_observations`, `memory_relations` (all bi-temporal with
  `valid_from` / `valid_to` / `superseded_by`); M:N joins
  `memory_entity_scope` and `memory_entity_tier` (with fuzzy
  weight); `memory_code_anchor` (cross-graph link with CHECK
  constraint requiring ≥ 1 FK populated); `memory_summary_tree`
  (RAPTOR, reserved for Phase 6.1); `memory_forget_log` and
  `memory_reflection_runs` (reserved for Phases 8 and 5). HNSW
  indices on `memory_observations.embedding` and
  `memory_summary_tree.summary_embedding`. All Phase-2 tables
  ship together so the bi-temporal invariants and FK relations
  are coherent at migration completion.
- **Phase 3.1 official-compat tools:** 9 MCP tools wired through
  `tool_memory_crud.rs` mirror
  `@modelcontextprotocol/server-memory` exactly:
  `memory_create_entities`, `memory_create_relations`,
  `memory_add_observations`, `memory_delete_entities`,
  `memory_delete_observations`, `memory_delete_relations`,
  `memory_read_graph`, `memory_search_nodes`, `memory_open_nodes`.
  All deletes are soft-deletes via `valid_to = NOW()` per the
  bi-temporal contract (decision 7). The scope-tuple
  `{user_id?, agent_id?, session_id?, project_id?}` is honored
  on every create / read / search call; defaults to workspace-wide.
- **Telemetry:** 9 new `AtomicU64` counters on `StatsTracker`
  (`memory_entities_created`, `_relations_created`,
  `_observations_added`, `_entities_deleted`,
  `_observations_deleted`, `_relations_deleted`,
  `_read_graph_calls`, `_search_nodes_calls`, `_open_nodes_calls`)
  + matching Prometheus exposition + JSON snapshot fields.
- **Tests:** 8 integration tests in
  `pgmcp-testing/tests/memory_phase2_3.rs` covering schema
  table-existence, bi-temporal column presence, CHECK enforcement
  on `memory_code_anchor`, CRUD round-trips (create → search →
  open → delete → re-read), soft-delete semantics, observation
  dedupe, and empty-input rejection. Inventory-coverage test
  (`query_inventory_vs_coverage`) passes — every dispatched memory
  tool has a corresponding integration test.
- **Verification gate:** `./scripts/verify.sh` green across all 8
  gates (preflight, fmt, clippy zero-warnings, debug build, debug
  tests, release gpu_fallback_smoke, every-tool-tested check,
  release tests).

### M3 — Memory graph SOTA-shaped (Phase 3.2 pgmcp extensions)

- **Status:** ✅ shipped.
- **Phase 3.2 query surface** (`src/db/queries.rs`):
  - `memory_semantic_search` — vector cosine over
    `memory_observations.embedding` (BGE-M3, 1024d). Strict
    dim-check rejects mis-sized inputs.
  - `memory_hybrid_search` — RRF fusion of FTS over observation
    content + dense vector. Per-subquery candidate pool sized at
    `3 × target_k`, fused with `1 / (rnk + 60)` (Cormack 2009).
  - `memory_facts_at` — bi-temporal point-in-time snapshot;
    entities + observations + relations filtered by
    `valid_from <= as_of AND (valid_to IS NULL OR valid_to > as_of)`.
  - `memory_relations_traverse` — depth-bounded BFS over
    `memory_relations` via recursive CTE; capped by `max_depth`
    (≤ 6) and `max_nodes` (≤ 1000).
  - Code-anchor CRUD: `memory_anchor_entity`,
    `memory_unanchor_entity`, `memory_find_code_for_entity`,
    `memory_find_entities_for_code` — bidirectional
    code-graph ↔ memory-graph cross-linking via
    `memory_code_anchor`.
- **Phase 3.2 MCP tools** (`src/mcp/tools/tool_memory_ext.rs`):
  8 new tools wired through `dispatch_tool!`:
  `memory_semantic_search`, `memory_hybrid_search`,
  `memory_facts_at`, `memory_relations_traverse`,
  `memory_anchor_entity`, `memory_unanchor_entity`,
  `memory_find_code_for_entity`, `memory_find_entities_for_code`.
  Tier filter validated against the 5 cognitive tiers; RFC3339
  parsing for `as_of`; clear error mapping
  (`sqlx::Error::Protocol` → `McpError::invalid_params`).
- **Tests:** 9 integration tests in
  `pgmcp-testing/tests/memory_phase3_2.rs` cover cosine-ranking,
  dim rejection, bi-temporal snapshot correctness (pre- vs
  post-delete), BFS depth-capping, anchor round-trip + reverse
  lookup, all-NULL anchor rejection, target-count enforcement on
  `find_entities_for_code`, tier-filter validation, and the
  hybrid-search dim-mismatch path.
- **Verification gate:** `./scripts/verify.sh` green across all 8
  gates.

<!-- Future milestone entries follow the same pattern. -->

---

## See also

- [`02-phases.md`](02-phases.md) — phase definitions and
  dependencies.
- [`07-risks-and-verification.md`](07-risks-and-verification.md) —
  the test surface that each milestone must satisfy.
- [`README.md`](README.md) — the doc-update discipline that requires
  this file to be appended to in every implementation PR.
- `docs/scientific-ledger/` — incident/debugging write-ups; link
  here from milestones when relevant.
