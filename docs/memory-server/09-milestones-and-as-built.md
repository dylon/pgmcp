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
- **Sub-step 3 (Phase 1 embedding upgrade):** ⏳ pending.

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
