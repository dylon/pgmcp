# 06 — MCP tool catalog

The set of MCP tools that the memory server adds, grouped by phase.
All tools accept an optional `scope` object and respect per-client
`OutputFormat` per Phase 10. Retrieval tools default to **handles**
(compact IDs); pass `expand=true` to inline content.

This file is the **as-built** tool reference — update it in the same
PR that registers each tool in `src/mcp/server.rs`.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §14.

---

## Conventions

- **Scope** object — `{user_id?, agent_id?, session_id?, project_id?}`.
  Default: `(current_user, NULL, NULL, current_project_or_NULL)`.
- **`as_of: timestamp`** — defaults to NOW(); bi-temporal point-in-
  time view.
- **`brief: bool` / `expand: bool`** — verbosity hints honored by
  every serializer.
- **`rerank: bool`** — opt-in cross-encoder rerank where applicable
  (Phase 7+).
- **`OutputFormat`** — chosen per client (Markdown for Claude Code,
  CompactJson for Codex, Text for generic).

---

## Phase 0 — Quick wins

- `recall_prompts(query: string, project?: string, session?: uuid, k=10)`
  — vector search over `session_prompts.embedding`. Returns
  `[{prompt_text, session_id, ts, similarity}]`.
- `search_mandates(query: string, polarity?: enum, scope?: enum, k=20)`
  — FTS over `durable_mandates.imperative || target` (text only
  until Phase 2 cutover; semantic after).

---

## Phase 3 — Official MCP memory-server compatible

These mirror `@modelcontextprotocol/server-memory` exactly so agents
that target the official server can swap pgmcp in unchanged.

- `memory_create_entities([{name, entity_type, observations[]}])`
- `memory_create_relations([{from, to, relation_type}])`
- `memory_add_observations([{entity_name, contents[]}])`
- `memory_delete_entities(names[])`
- `memory_delete_observations([{entity_name, observations[]}])`
- `memory_delete_relations([{from, to, relation_type}])`
- `memory_read_graph(scope?)` — full dump (scope-filtered for safety)
- `memory_search_nodes(query: string, scope?)` — substring/ILIKE
- `memory_open_nodes(names[])`

---

## Phase 3 — pgmcp extensions

- `memory_semantic_search(query, scope?, tier?, k=20, rerank?=false)`
  — vector search over `memory_observations.embedding`.
- `memory_hybrid_search(query, scope?, tier?, k=20, rerank?=false)`
  — RRF: FTS over content + BM25 + dense vector.
- `memory_facts_at(timestamp, scope?, tier?, entity_filter?)` —
  bi-temporal point-in-time view.
- `memory_relations_traverse(seed_entity_id, max_depth=2,
  relation_filter?, scope?)` — BFS over `memory_relations`,
  scope/time-filtered.

---

## Phase 3 — Code crosslink (decision 9)

- `memory_anchor_entity(entity_id, file_id? | chunk_id? | topic_id?,
  anchor_type)` — create a `memory_code_anchor` row.
- `memory_unanchor_entity(anchor_id)` — delete one.
- `memory_find_code_for_entity(entity_id, anchor_type?)` — list code
  anchors.
- `memory_find_entities_for_code(file_id? | chunk_id? | topic_id?)`
  — list entities anchored to a code object.

---

## Phase 5 — Reflection

- `memory_reflect(scope?, since?, max_observations=200)` —
  agent-driven reflection cycle. Returns structured summary +
  written observations. (Cron variant requires no MCP surface.)

---

## Phase 6 — Hierarchical & graph-enhanced retrieval

- `memory_raptor_search(query, scope?, k=10, levels?: int[])` —
  Phase 6.1, multi-level RAPTOR retrieval.
- `memory_ppr_search(query, scope?, k=20, alpha=0.85, max_seeds=10)`
  — Phase 6.2, HippoRAG-style Personalized PageRank.
- `memory_unified_search(query, node_types?, scope?, k=20)` — Phase
  6.3, vector retrieval over `memory_unified_nodes` (heterogeneous
  types: memory_entity, observation, chunk, topic, durable_mandate,
  commit).
- `memory_neighbors(node_id, node_type, depth=1, edge_filter?,
  node_filter?)` — Phase 6.3, BFS expansion across types via
  `memory_unified_edges`.
- `memory_path_search(query, source_filter?, target_filter?,
  max_hops=3, k=10, prune=true)` — Phase 6.4, ranked relationship
  paths with PathRAG flow-pruning.

**Empirical-gating discipline:** all Phase 6 tools are opt-in at the
call site. Vanilla `memory_semantic_search` (Phase 3) remains the
default. See [`02-phases.md`](02-phases.md) Phase 6.5 and the risk
register in [`07-risks-and-verification.md`](07-risks-and-verification.md).

---

## Phase 7 — Reranker

No net new tool unless we expose
`rerank_candidates(query, candidates[])` standalone for debugging.
Instead, adds `rerank: bool` flag to existing search tools:

- `memory_semantic_search(..., rerank=true)`
- `memory_hybrid_search(..., rerank=true)`
- `memory_unified_search(..., rerank=true)`
- `memory_path_search(..., rerank=true)`
- `hybrid_search(..., rerank=true)` — existing tool, now rerankable
- `memory_raptor_search(..., rerank=true)`
- `memory_ppr_search(..., rerank=true)`

---

## Phase 8 — Forget

- `memory_forget(target_type, target_id, cascade=false)` —
  soft-delete by default; `cascade=true` hard-deletes with full
  audit manifest written to `memory_forget_log`.
- `memory_purge_expired(window_days?, dry_run=true)` — admin-only;
  lists what `memory-retention` cron would delete.

---

## Phase 9 — Evaluation (not user-facing)

Internal harness only; cron job writes per-scenario pass/fail and
recall/precision metrics to `pgmcp_metadata`. See Phase 9 in
[`02-phases.md`](02-phases.md).

---

## Phase 10 — Client profile (handled transparently)

No net new tool. Every tool above respects:

- Client detected from `clientInfo.name` on MCP `initialize`.
- `OutputFormat` enum selected per client.
- Per-(client, tool) description overrides resolved from
  `assets/client_profiles.toml`.
- Per-call `brief=true` / `expand=true` flags honored.

---

## See also

- [`02-phases.md`](02-phases.md) — phase-by-phase plan that
  introduces each tool.
- [`05-schema.md`](05-schema.md) — SQL tables / views these tools
  query.
- [`03-architecture.md`](03-architecture.md) — backend traits the
  tools call into.
- [`08-configuration.md`](08-configuration.md) — TOML keys that
  affect tool behavior (defaults, latency caps, auto-disable).
