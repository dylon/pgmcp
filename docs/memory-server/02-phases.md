# 02 — Implementation phases

This document is the engineering plan, phase by phase. Every phase is
**additive** — pgmcp's existing tools and tables remain untouched
throughout. The memory server is a **new layer** that cross-links
with existing pgmcp graphs (code, topics) via dedicated anchor
tables.

The 13 committed decisions ([`01-decisions.md`](01-decisions.md))
shape every phase. The schema is in [`05-schema.md`](05-schema.md);
the MCP tool catalog is in [`06-tools.md`](06-tools.md).

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §7.

---

## Ordering constraints

- Phase 1 (embeddings) must come before Phase 2 (memory schema), per
  decision 5. Building memory tables on 384d would force a second
  reindex.
- Phase 3 (CRUD tools) depends on the Phase 2 schema.
- Phase 4 (LLM extractor) depends on Phase 3 (writes target the new
  tools).
- Phases 6–9 may interleave once 1–5 are in place.

---

## Phase 0 — Quick wins (no new schema, no new dependencies)

Three near-free improvements unblocked by the
[gap-analysis audit](00-context-and-gap.md#5-what-pgmcp-already-has).

- **Surface `session_prompts.embedding`.** Populated on every prompt;
  HNSW index already exists (see
  [`00-context-and-gap.md`](00-context-and-gap.md) §5.5); zero
  readers today. Add MCP tool
  `recall_prompts(query, project?, session?, k=10)` doing vector
  search over the existing column.
- **Search-over-durable-mandates.** `durable_mandates` has one
  reader, no search surface. Add MCP tool
  `search_mandates(query, polarity?, scope?, k=20)` — Postgres FTS
  over `imperative || target` first; add a
  `mandate_embedding VECTOR(d)` column in Phase 2 once embeddings
  are upgraded.
- **Mandate supersession.** `MandateStatus::Superseded` exists but no
  code path sets it. Add a consolidation step in the existing
  session observation pipeline: when a new mandate is inserted and
  an active mandate exists with `lower(imperative)` Levenshtein ≤ 3,
  mark the prior as `Superseded` and link via `source_mandate_id`.
  No LLM needed.

**Deliverables:** 2 MCP tools + 1 background hook. **No schema
change.**
**Verification:** `scripts/verify.sh` + new integration tests in
`pgmcp-testing/tests/memory_phase0.rs`.

---

## Phase 1 — Embedding upgrade (BGE-M3 + Matryoshka)

**Decision (5):** switch embeddings first, before any memory tables,
so the new tables never see a 384d column.

**Model choice:** **BGE-M3** (`BAAI/bge-m3`).

- 568M params, 1024d output, Matryoshka-truncatable to
  64/128/256/512/1024.
- Dense + sparse + multi-vector in one model; we adopt dense
  initially, sparse later as a Phase 8 rerank input.
- 100+ languages; strong on code and prose.
- Fits in 1.2 GB VRAM (fp16). Leaves ~6.8 GB of 8 GB for the LLM
  extractor + reranker.

**Why not Qwen3-Embedding or Voyage-code-3:** see
[`04-hardware.md`](04-hardware.md).

**Architecture — `Embedder` trait** (new `src/embeddings/mod.rs`).
Signature in [`03-architecture.md`](03-architecture.md).

Mirrors the `FcmBackend` swap-seam pattern (`src/fcm/mod.rs`). MiniLM
stays as a temporary second impl so we can dual-write during the
migration; remove after Phase 1 is complete.

**Schema changes** (no destructive migration):

- Add columns `embedding_v2 VECTOR(1024)` and
  `embedding_signature TEXT` to `file_chunks` and `session_prompts`.
- Add HNSW index on `embedding_v2` (m=24, ef_construction=200,
  ef_search=100). Keep the existing 384d index live until cutover.
- Extend `pgmcp_metadata` HNSW tracking to key on
  `(table_name, embedding_signature, m, ef_construction)`.

Full SQL in [`05-schema.md`](05-schema.md) §12.1.

**Migration cron** (new `src/cron/embedding_migration.rs`):

- Scans for rows with `embedding_v2 IS NULL`, batches 256 at a time,
  embeds with BGE-M3, writes back.
- Configurable `embedding_migration_interval_secs` (default 600).
- Stats counters: `embeddings_migrated`,
  `embeddings_migration_errors`.

**Cutover** (manual, behind config flag
`vector.active_signature`):

- Once `WHERE embedding_v2 IS NULL` returns zero for both tables,
  flip `active_signature = "bge-m3-v1"`.
- All queries route to `embedding_v2`.
- Drop `embedding` column in a follow-up cleanup phase (kept for one
  release as a rollback escape).

**Storage cost:** ~3× growth on embedding columns. At 21k indexed
files × ~5 chunks/file × 1024d × 4 bytes ≈ 420 MB + HNSW (~2× =
840 MB). Acceptable given the 4 TB primary disk.

**Verification:** `scripts/verify.sh` + a benchmark in
`pgmcp-testing/tests/embedding_bench.rs` comparing query quality
(MTEB-style synthetic) and latency between MiniLM and BGE-M3 on a
held-out set. Migration end-to-end test using a small project
fixture.

---

## Phase 2 — Memory schema (scope + taxonomy + bi-temporal + code anchor)

**Decisions baked in:** (1) scope + taxonomy via join tables; (3)
bi-temporal everything; (8) multi-agent shared memory; (9)
code-graph cross-linking.

See [`05-schema.md`](05-schema.md) for full SQL. Tables introduced:

1. `memory_scope` — dimensions (`user_id`, `agent_id`, `session_id`,
   `project_id`). Each nullable; NULL = "any". UNIQUE on the tuple.
   A scope row is created once and reused.
2. `memory_entities` — name + entity_type + bi-temporal columns
   (`valid_from`, `valid_to`, `superseded_by`) + importance + source.
3. `memory_entity_scope` — M:N join (entity ↔ scope). Many-scope per
   entity supports shared memory (decision 8) without copying rows.
4. `memory_entity_tier` — M:N join (entity ↔ cognitive tier). Tiers
   from the cognitive taxonomy: `working | episodic | semantic |
   procedural | reflective`. Fuzzy weights allow an entity to be
   primarily semantic but also episodic.
5. `memory_observations` — content + 1024d BGE-M3 embedding +
   bi-temporal + source + importance. Content sha256 deduplication.
6. `memory_relations` — typed edges, bi-temporal,
   `(from_entity_id, to_entity_id, relation_type, valid_from)`
   UNIQUE so re-asserting an invalidated relation creates a new row
   instead of overwriting.
7. `memory_code_anchor` — entity ↔ (file | chunk | topic) with
   `anchor_type` (e.g. `implements`, `tested-by`, `documented-in`,
   `caused-by`, `applies-to`). At least one of the three FKs must be
   non-NULL.

HNSW index on `memory_observations.embedding`; standard b-tree
indices on the FK columns + `(valid_from, valid_to)` partial indices
for point-in-time queries.

**Migration discipline** — `src/db/migrations.rs` is append-only.
All new tables added in a single migration with a coordinated
`CHECK` constraint set; no destructive ALTER on existing tables.

---

## Phase 3 — Memory CRUD + retrieval tools

**Decision (2):** wire per-client output formatting (see Phase 10).

### 3.1 Official-server-compatible tools

Mirror `@modelcontextprotocol/server-memory` exactly so agents that
target the official server can swap pgmcp in unchanged:

- `memory_create_entities` — array of
  `{name, entityType, observations[]}`
- `memory_create_relations` — array of `{from, to, relationType}`
- `memory_add_observations` — array of `{entityName, contents[]}`
- `memory_delete_entities` — names[]
- `memory_delete_observations` — `{entityName, observations[]}[]`
- `memory_delete_relations` — array of `{from, to, relationType}`
- `memory_read_graph` — dump (scope-filtered for safety)
- `memory_search_nodes` — substring/ILIKE
- `memory_open_nodes` — names[]

Default scope: `(user_id=current_user, agent_id=NULL,
session_id=NULL, project_id=current_project_or_NULL)`. Overridable
via tool params.

### 3.2 pgmcp extensions

- `memory_semantic_search(query, scope?, tier?, k=20)` — vector
  search over `memory_observations.embedding` (BGE-M3).
- `memory_hybrid_search(query, scope?, tier?, k=20)` — RRF: FTS over
  content + BM25-style ranking + dense vector. Re-uses the
  `hybrid_search` plumbing from existing code search.
- `memory_facts_at(timestamp, scope?, tier?)` — bi-temporal view;
  returns the graph state as-of-`t`. Implemented via
  `WHERE valid_from <= $1 AND (valid_to IS NULL OR valid_to > $1)`.
- `memory_relations_traverse(seed_entity_id, max_depth,
  relation_filter?)` — BFS over `memory_relations`, scope/time-
  filtered.
- `memory_anchor_entity(entity_id, file_id | chunk_id | topic_id,
  anchor_type)` — link memory ↔ code-graph.
- `memory_find_code_for_entity(entity_id)` /
  `memory_find_entities_for_code(file_id | chunk_id)` —
  bidirectional cross-graph queries.

All extension tools accept a `scope` object
(`{user_id?, agent_id?, session_id?, project_id?}`) and `as_of?`
timestamp (defaults to NOW()).

### 3.3 Quick-win promotions from Phase 0

- `search_mandates` (Phase 0 text) gains a `--semantic` mode that
  hits the new `durable_mandates.embedding` column once Phase 2
  backfills it.
- `recall_prompts` (Phase 0) likewise gains semantic search via
  `session_prompts.embedding_v2` after Phase 1 migration completes.

---

## Phase 4 — Local LLM extractor (salience pipeline replacement)

**Decision (4):** SOTA local model on the user's hardware.

**Model choice:** **Qwen3-8B-Instruct, Q4_K_M GGUF** loaded via
candle.

- Q4_K_M quantized footprint: ~4.8 GB VRAM (model) + ~0.5 GB (KV
  cache for a 4 K context window).
- Total resident budget when LLM is active: BGE-M3 (1.2 GB) +
  Qwen3-8B Q4 (5.3 GB) ≈ 6.5 GB, leaving headroom under the 8 GB
  ceiling. The reranker (Phase 7, ~0.6 GB at fp16) is loaded only
  during reranks, and the LLM is unloaded for those windows.
- Strong instruction-following at the 8B class; SOTA on extraction
  benchmarks for its size as of late 2025.

**Fallback model:** Qwen3-4B-Instruct Q4_K_M (~2.5 GB) — selected
when config sets `[memory.extractor] model = "qwen3-4b"` or VRAM
probe at startup shows < 6 GB free.

**Why not cloud Haiku 4.5:** user wants local. We do leave an
optional `[memory.extractor] backend = "cloud"` path for future
opt-in; the trait makes it a config flip, not a refactor.

**Architecture — `LlmExtractor` trait** (`src/llm/mod.rs`).
Signature in [`03-architecture.md`](03-architecture.md).

Same FcmBackend pattern: trait + closed-set enum + factory.

**Pipeline integration** (`src/api/handlers.rs`):

- `POST /api/session/observe` continues to ingest prompts (no API
  change).
- The existing regex pipeline becomes **Stage A** — fast, runs
  inline, populates `session_mandates` (preserving today's behaviour
  and SLO).
- New **Stage B** — a background worker (new
  `src/sessions/extractor_worker.rs`) batches recent prompts, calls
  the `LlmExtractor`, writes results into `memory_*` tables under
  the session's scope.
- Stage B runs at most once per N seconds per session (configurable
  `[memory.extractor] inline_debounce_secs = 30`) to keep VRAM
  contention bounded.
- On contradiction detection, the worker sets `valid_to = NOW()` on
  the prior fact and inserts the new one with `superseded_by`
  pointing to the old row (bi-temporal invalidation, decision 3).

**Prompt template** (versioned in source):

```
You extract structured facts from user prompts to a coding agent.
Given the prompt and existing relevant entities, return JSON matching
the schema below. Only emit facts the user actually states; do not
invent. Mark contradictions explicitly.
[schema]
[existing entities]
[user prompt]
```

Output JSON validated against `schemars`-derived JSON schema;
rejection on parse failure (no silent garbage in `memory_*` tables).

**Promotion path:** `promote_session_mandate` now writes into
`memory_entities` + `memory_observations` *in addition to*
`durable_mandates`. The CLAUDE.md marker-section behaviour is
preserved.

---

## Phase 5 — Reflection (agent-driven + cron)

**Decision (6):** both.

### 5.1 Agent-driven

MCP tool `memory_reflect(scope?, since?, max_observations=200)`:

- Selects recent observations under `scope` since `since` (default:
  24h).
- Calls the `LlmExtractor` with the reflection prompt (separate
  template).
- Reflection prompt: "Given these recent facts, identify
  higher-order patterns, preferences, or summaries worth
  remembering. Cite the source observations by ID."
- Writes new observations with `source = "reflection"` and a JSON
  field `derived_from = [obs_ids]` for provenance.
- Returns a structured summary so the agent can decide whether to
  act on it.

### 5.2 Cron-driven

New cron job `memory-reflect` (`src/cron/memory_reflect.rs`):

- Configurable interval (`[cron] memory_reflect_interval_secs`,
  default 86400 = daily).
- Default **off** (`enabled = false`) to control LLM cost/VRAM
  contention.
- When on, iterates scopes with > N new observations since the last
  reflection, calls the same reflection logic per scope.
- Tracks last-reflection timestamps in `pgmcp_metadata`.

---

## Phase 6 — Hierarchical & graph-enhanced retrieval

**Goal:** beat flat ANN on thematic / multi-hop / global queries.

### 6.1 RAPTOR summary tree (`memory_summary_tree`)

- Level 0 = leaf, points to one `memory_observation`.
- Level k+1 = LLM-summarized cluster of ≤ N level-k nodes (FCM
  clustered on embeddings; reuses the existing CUDA FCM).
- New cron `memory-raptor` builds/refreshes the tree per scope on a
  schedule (default 12h).
- New tool `memory_raptor_search(query, scope?, k=10)`:
  - Embed query.
  - Top-k against each level; merge results across levels by RRF.
  - Returns ranked candidates with their levels (caller can pick
    abstraction).

### 6.2 HippoRAG-style Personalized PageRank

- The entity/relation graph already has the right shape.
- New tool `memory_ppr_search(query, scope?, k=20, alpha=0.85)`:
  - Embed query.
  - Top-k seed entities by cosine on observation embeddings linked
    to each entity.
  - Run Personalized PageRank from those seeds over
    `memory_relations` (petgraph already in deps).
  - Return top-PPR entities + their top observations.

### 6.3 Heterogeneous-node graph view (NodeRAG-inspired)

**Reference:** NodeRAG (Xu et al., arXiv:2504.11544, Apr 2025).

**Why for pgmcp:** the unique asset is the existing zoo of node
types (file · chunk · topic · memory_entity · mandate · commit)
cross-linked across two graphs. Formalize them in a single unified
node/edge view so retrieval can traverse across types in one query.

**Schema additions** (views, **no new base tables** — full DDL in
[`05-schema.md`](05-schema.md) §12.2):

- `memory_unified_nodes` (materialized view) — UNION ALL projection
  over `memory_entities`, `memory_observations`, `file_chunks`,
  `code_topics`, `durable_mandates`, `git_commits`. Common columns:
  `(node_id, node_type, label, embedding, scope_id, importance)`.
- `memory_unified_edges` (view) — UNION ALL over `memory_relations`,
  `memory_code_anchor`, `code_graph_edges`,
  `chunk_topic_assignments`. Common columns:
  `(from_id, from_type, to_id, to_type, edge_type, weight)`.

Materialized view refresh on the same cadence as the existing
`similarity-scan` cron (cheap; UNION ALL of indexed tables).

**Tools:**

- `memory_unified_search(query, node_types?: enum[], scope?, k=20)`
- `memory_neighbors(node_id, node_type, depth=1, edge_filter?,
  node_filter?)`

### 6.4 Path-based retrieval (PathRAG-inspired)

**Reference:** PathRAG (Chen et al., arXiv:2502.14902, Feb 2025).

**Why:** the paper's thesis — *redundancy, not insufficiency, is the
real problem in graph-enhanced RAG* — directly answers the failure
mode that cross-graph traversal would otherwise create for pgmcp
(context bloat from over-expanded neighbor sets).

**Tool:**

```
memory_path_search(
  query: string,
  source_filter?: { node_types?, scope? },
  target_filter?: { node_types?, scope? },
  max_hops: int = 3,
  k: int = 10,
  prune: bool = true,         // PathRAG flow-based pruning
)
  → [{ path: [{id, type, label}], edges: [{type, weight}],
       score: { vector_term, path_penalty, edge_product } }]
```

Algorithm:

1. Seed: top-k from `memory_unified_search(query)`.
2. Expand: BFS from each seed up to `max_hops` over
   `memory_unified_edges`.
3. Score each path:
   `score = α · cos(q, last_node) − β · hops + γ · Σ edge_weight`.
4. Prune (when `prune=true`): for paths that share a prefix node,
   keep only the highest-scored variant; drop paths whose marginal
   information content (Jaccard of node sets vs already-kept paths)
   exceeds a threshold. This is the PathRAG flow-pruning idea,
   simplified.
5. Return top-`k` paths.

Output renders to Markdown for Claude Code and CompactJson for
Codex per the Phase 10 client profile.

VRAM impact: none beyond Phase 1 embedder.

### 6.5 Empirical gating — graph retrieval is not free

**Reference:** GraphRAG-Bench (Han et al., arXiv:2506.02404, Jun
2025). Headline: GraphRAG-class methods *often underperform vanilla
RAG* — −13.4% on Natural Questions, −16.6% on time-sensitive queries;
only +4.5% on HotpotQA multi-hop at 2.3× latency. Graph retrieval is
a workload-conditional optimization, not a free win.

**Discipline baked into the plan:**

- All graph-enhanced tools — `memory_ppr_search`,
  `memory_raptor_search`, `memory_unified_search`,
  `memory_neighbors`, `memory_path_search` — are **opt-in at the
  call site**. `memory_semantic_search` (Phase 3) remains the
  default retrieval path.
- Phase 9's internal eval harness adds an "A/B retrieval" scenario
  family: same query, vanilla vs. graph variant, recorded as
  `(vanilla_score, graph_score, delta, latency_delta)` in
  `pgmcp_metadata`.
- New Prometheus counter
  `pgmcp_graph_retrieval_underperformance_total{tool="..."}`
  increments each time a graph variant scores strictly worse than
  vanilla on a scenario.
- Latency budget per call documented: `memory_semantic_search`
  < 100 ms; graph variants 200–500 ms target, hard-cap configurable
  in `[memory.graph_rag] max_latency_ms`.

---

## Phase 7 — Cross-encoder reranker

**Model:** **BGE-reranker-v2-m3** (`BAAI/bge-reranker-v2-m3`), 568M
params, ~600 MB fp16. Pairs naturally with BGE-M3.

**Architecture — `Reranker` trait** (`src/reranker/mod.rs`).
Signature in [`03-architecture.md`](03-architecture.md).

**Integration:**

- New optional `rerank = true` param on `memory_semantic_search`,
  `memory_hybrid_search`, the existing `hybrid_search`, and the
  Phase 6 tools.
- When on, top 50 candidates from the underlying retriever are
  reranked to top 10.
- VRAM: the reranker is loaded on first use, evicted after
  `reranker_idle_secs` (default 300). LLM extractor is unloaded
  during rerank windows to fit under the 8 GB ceiling.

---

## Phase 8 — Eviction & consolidation

**Decision (7):** soft-delete by default with retention window;
hard-delete behind explicit `cascade=true`.

### 8.1 Soft-delete (already free under bi-temporal)

A "deleted" fact = `valid_to = NOW()`. It stays in the DB and is
still retrievable via `memory_facts_at(t < deletion_time)`. The
default query path filters `WHERE valid_to IS NULL` so soft-deleted
rows don't contaminate retrieval.

### 8.2 Retention window cron

New cron `memory-retention` (`src/cron/memory_retention.rs`):

- Configurable `[memory.retention] window_days` (default 90).
- Hard-deletes rows where `valid_to < NOW() - window_days` *and*
  `importance < threshold` *and* no `superseded_by` chain extends
  from it.
- Configurable `enabled = true|false`; default true.

### 8.3 Consolidation (near-dup merge)

New cron `memory-consolidate`:

- Clusters near-duplicate observations under a scope (cosine ≥ 0.95).
- LLM-judges whether to merge (uses the same `LlmExtractor` with a
  merge prompt).
- On merge, the cluster's facts get a new "consolidated" observation
  with `derived_from = [obs_ids]`; the originals are
  `valid_to = NOW()`-soft-deleted (preserving the audit trail).

### 8.4 Explicit forget

MCP tool `memory_forget(entity_id | observation_id, cascade=false)`:

- `cascade=false` (default): soft-delete only.
- `cascade=true`: hard-delete the row *and* any rows whose
  `superseded_by` chain leads here, *and* any dependent
  `memory_code_anchor` / `memory_entity_scope` /
  `memory_entity_tier` rows. Returns a manifest of deleted rows for
  audit.
- Always logs a `memory_forget_log` entry (separate audit-only
  table) with `actor`, `target`, `cascade`, `timestamp`.

---

## Phase 9 — Internal evaluation harness

**Decision (10):** internal eval primary; public benchmarks
secondary.

### 9.1 Internal scenarios

`pgmcp-testing/tests/memory_eval.rs` defines scenarios as Rust data:

```rust
struct MemoryScenario {
    name: &'static str,
    setup: Vec<MemoryAction>,        // create entities/relations/observations
    queries: Vec<MemoryQuery>,
    expected: Vec<ExpectedResult>,   // recall ≥ X, contradiction detected, etc.
}
```

Initial scenario suite (≥ 20 scenarios across):

- **Recall** — fact stored in session 1 retrievable in session 2.
- **Contradiction** — user changes preference; the new fact is
  active and the prior is `valid_to` set.
- **Multi-hop** — "what languages does the user use for projects
  that depend on X" exercises relation traversal.
- **Cross-graph** — "what file implements concept Y" exercises
  `memory_code_anchor`.
- **Scope isolation** — agent A's private memory is not visible to
  agent B (decision 8).
- **Tier filtering** — `tier=procedural` filters out semantic facts.
- **Forgetting** — `memory_forget(... cascade=true)` actually
  removes every dependent row.
- **Reflection** — running `memory_reflect` produces at least one
  higher-order observation linked to source observations.

### 9.2 Cron-driven scoring

New cron `memory-eval` (`src/cron/memory_eval.rs`):

- Default **off** (`enabled = false`).
- When on, runs the scenario suite against a sandbox database
  (separate schema) and writes per-scenario pass/fail +
  recall/precision metrics to `pgmcp_metadata`.

### 9.3 Public benchmarks (secondary)

- LongMemEval-S and LoCoMo subsets adapted as additional scenarios.
- Treat absolute numbers skeptically (per
  [`00-context-and-gap.md`](00-context-and-gap.md) §4.8); use them
  only for relative direction tracking.

---

## Phase 10 — Client-specific protocol customization

**Decision (2):** efficient per-client; customize where the spec
allows.

The MCP protocol itself is fixed (we honour it). What we can
customize:

- **Client detection** — MCP `initialize` carries `clientInfo.name`
  and `clientInfo.version`. We record it per-connection.
- **Output format per client** — new enum
  `OutputFormat { Markdown, CompactJson, Text }` selected per
  request based on client + tool annotations.
  - Claude Code → Markdown (renders well in its UI; carries hint
    blocks).
  - Codex CLI → CompactJson (token-efficient, structured for
    downstream re-prompting).
  - Generic → Text (plain, no formatting).
- **Tool descriptions per client** — registry of
  `(client_name, tool_name) → description` overrides, falling back
  to a generic description. Used for nudging clients with different
  prompt-following habits (Claude wants explicit `Important:`-style
  callouts; Codex prefers terse imperative).
- **Default-compact, opt-in-expand outputs** — every retrieval-
  shaped tool returns **handles** by default
  (`{entity_id, name, entity_type, observation_ids[],
  anchor_ids[]}`), not full content. Agents that need text issue a
  follow-up `memory_open_nodes(ids[])` /
  `memory_get_observations(ids[])`. A per-call `expand=true` flag
  inlines content when the agent is sure it wants it. Motivated by
  the RecursiveMAS (arXiv:2604.25917) finding that text-mediated
  multi-agent collaboration burns 1.4–4.7× more tokens than it
  needs to — even though the paper's latent-space remedy doesn't
  apply through MCP (see [`10-alternatives.md`](10-alternatives.md)),
  the underlying token-budget pressure is real for any tool-using
  agent.
- **Verbosity hints** — extra optional MCP tool params
  (`brief=true`, `expand=true`) that all tools accept and downstream
  serializers honour.
- **Provenance inclusion** — Claude Code requests get full
  provenance (source session, prompt, anchor); Codex gets
  entity/observation IDs only.

Implementation: `src/mcp/client_profile.rs` defines a `ClientProfile`
struct loaded from a small `assets/client_profiles.toml`. New tool
output goes through `serialize_for(client_profile, output)`.

---

## Phase 11 — Internal latent-space pipeline (RecursiveMAS-style, hardware-gated)

**Decision (12):** adopt RecursiveMAS hidden-state hand-off **inside
pgmcp's own LLM pipeline** when hardware supports it. Skips
text-decode → re-tokenize → re-embed cycles between same-backbone
stages (Phase 4 extract → Phase 5 reflect → Phase 8 consolidate).

This is an *internal* optimization. The MCP wire to Claude Code /
Codex stays text/JSON (Phase 10 covers external token efficiency).
Phase 11 targets the inference cost *inside* pgmcp, where the user's
own GPU pays the bill.

**Reference:** Yang et al., *Recursive Multi-Agent Systems*,
arXiv:2604.25917 (Apr 2026). The paper's RecursiveLink — a 2-layer
residual projection — is the mechanism. We use **inner-RecursiveLink
only** (same-backbone agents in a loop); no outer-link is needed
because we don't bridge model architectures inside pgmcp.

### 11.1 Architecture

Same Qwen3-8B-Instruct Q4 backbone selected in Phase 4. The forward
pass for stage *k* writes its last-layer hidden states `h⁽ᵏ⁾` to a
ring buffer. Stage *k+1* takes:

```
input_embeddings⁽ᵏ⁺¹⁾ = R_in(h⁽ᵏ⁾) ⊕ Embed(stage_k+1_prompt_prefix)
R_in(h) = h + W_2 · σ(W_1 · h)         // residual; W_3 fixed at I
```

where `(W_1, W_2)` are the trained RecursiveLink matrices (~13M
params total) and `σ` is GELU. The result is the next stage's input
embedding sequence, replacing the conventional
`tokenize(decode(h⁽ᵏ⁾))` round-trip.

Final stage decodes to text as usual so the agent-visible output
(entities, observations, contradictions) is unchanged.

**Trait** (`src/llm/latent_pipeline.rs`). Signature in
[`03-architecture.md`](03-architecture.md).

The text-mediated pipeline from Phase 4 stays alive as a fallback
path. On any of: VRAM probe failure, RecursiveLink weights missing,
JSON schema validation failure on the latent output, the dispatcher
falls back to the text path and logs the event for telemetry.

### 11.2 Training RecursiveLink (one-shot)

The matrices `(W_1, W_2)` are pgmcp-specific — they're trained to
align Qwen3-8B's last-layer hidden states with the next stage's
input embedding distribution under our prompt templates. Trained
**once**, then versioned (`link_signature`) and stored as a small
safetensors file shipped with pgmcp (or generated by the user).

**Training set construction:**

- Take ~5k–10k recent `session_prompts` rows (deduped).
- For each, run the existing text-mediated extract→reflect pipeline
  to generate the "gold" intermediate text from stage 1.
- The pair `(h⁽¹⁾, Embed(gold_text))` is one training example.
- Loss: `1 − cos(R_in(h), Embed(gold))` (the regression objective
  from the paper, eq. 5).

**Compute budget on RTX 4060 Ti 8 GB:** see
[`04-hardware.md`](04-hardware.md) for the VRAM table.

**Training recipe:**

- ~3 epochs over 10 k samples ≈ 30k steps.
- AdamW, lr 5e-4 (per paper).
- Wall time on RTX 4060 Ti: estimated ~3–6 hours; we accept the
  wait.
- Optional cloud-burst alternative documented: rent one A100
  instance on a hyperscaler for ~1 hour (~$2–5) to compress
  training to ~30 minutes. Inference is local either way.

### 11.3 Hardware gating

Startup probe (`src/llm/dispatcher.rs`):

1. Read `[memory.latent_pipeline] enabled` (default `false`).
2. If `true`, run a 30-second VRAM probe — load Qwen3-8B + BGE-M3 +
   RecursiveLink weights; run one forward pass on a synthetic
   prompt.
3. On success: dispatcher routes pipeline stages through
   `LatentPipeline`.
4. On failure (OOM, missing weights, schema validation regression
   beyond a configurable threshold): log a structured warning,
   downgrade to text-mediated pipeline, set
   `pgmcp_metadata.latent_pipeline_active = 'false'`.

The user can re-enable manually after fixing the underlying issue;
no automatic retry-loop.

### 11.4 Quality validation harness

Latent pipeline must not regress extraction quality vs
text-mediated. New cron `latent-pipeline-quality` runs once per day
when enabled:

- Sample 50 recent prompts.
- Run both pipelines (latent + text) on the same inputs.
- Compute exact-match on extracted entity names + Jaccard on
  relations + cosine on observation embeddings.
- Write `(latent_score, text_score, delta)` per sample to
  `pgmcp_metadata`.
- If `delta > regression_threshold` (configurable, default −0.05)
  over a 7-day window, automatically downgrade to text pipeline and
  emit a Prometheus alert.

### 11.5 Token / latency telemetry

Every pipeline run emits counters:

- `pgmcp_pipeline_tokens_saved_total{stage="extract_to_reflect"}`
- `pgmcp_pipeline_latency_seconds{path="latent" | "text"}`
- `pgmcp_pipeline_fallback_total{reason="vram_oom" |
  "schema_invalid" | …}`

A `/api/memory/pipeline_stats` REST endpoint exposes the per-stage
latency + token-saving summary so the user can quantify the
optimization's value.

### 11.6 Out of scope (deferred to later phases)

- **Outer RecursiveLink (cross-backbone).** Only relevant if pgmcp
  later runs a heterogeneous pipeline (e.g., a small Phi-class
  scorer in front of Qwen3-8B). Not needed for the all-Qwen
  pipeline.
- **End-to-end inner+outer training (the paper's "outer loop").**
  Skipped because we have one backbone — inner-link alone covers
  it.
- **Training-data feedback loop** (re-train on the user's own
  extractions over time). Plausible Phase 11b once Phase 11 has
  shipped and we have a quality-score time series to optimize
  against.

---

## Cross-phase concerns

- **No `[features]` table.** All backend swaps are traits +
  closed-set enums, per the
  `feedback_feature_gated_build_verification.md` discipline.
- **CUDA mandatory.** Every new compute path (embedder, extractor,
  reranker, RAPTOR summarization, latent pipeline) defaults to CUDA
  via candle; a CPU fallback returns degraded-mode results (smaller
  batches, longer latencies) but never feature-gates.
- **Verification gate.** Every phase ends with `scripts/verify.sh`
  green, no new clippy warnings, integration tests added in
  `pgmcp-testing/`.
- **Hooks discipline.** Per
  `feedback_subagents_skip_parent_hooks.md`, anything model-side
  (additionalContext, descriptions) is best-effort; harness-
  enforced surfaces (the new tools, scope filters) carry the load.
- **Token efficiency is a first-class concern, two surfaces:** see
  decisions 11 and 12 in [`01-decisions.md`](01-decisions.md).

---

## See also

- [`03-architecture.md`](03-architecture.md) — backend trait surfaces
  referenced by Phases 1, 4, 7, 11.
- [`05-schema.md`](05-schema.md) — full SQL for Phase 1's columns
  and Phase 2's tables and Phase 6.3's views.
- [`06-tools.md`](06-tools.md) — the MCP tool catalog grouped by
  phase.
- [`07-risks-and-verification.md`](07-risks-and-verification.md) —
  the risk register + per-phase verification table.
- [`09-milestones-and-as-built.md`](09-milestones-and-as-built.md) —
  M1–M7 milestone groupings + as-built log.
