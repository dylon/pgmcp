# ADR-002: Extend pgmcp into a SOTA memory server (additive layer)

**Status:** Accepted
**Date:** 2026-05-18

## Context

Claude Code, Codex, and similar coding agents have bounded context
windows. Long-running projects, repeated user preferences, prior
architectural decisions, and incident history all live *outside* the
window. The MCP ecosystem has converged on the **memory server**
pattern: an external service the agent reads/writes via tool calls so
effective memory is bounded by storage, not context.

pgmcp already does much of what a memory server does (durable vector
store, semantic search over a "claude" synthetic project of past
sessions, session-mandate observation), but it was built code-indexer-
first and lacks the structural pieces a SOTA memory server provides:
an entity/relation graph of user facts, LLM-driven salience, a
frontier embedding model, a cross-encoder reranker, hierarchical
retrieval, an eviction policy, and explicit forgetting.

## Decision

**Extend pgmcp with a memory-server layer.** The implementation is
fully additive — no existing tools, tables, or cron jobs are
modified or removed. The full design lives in
[`docs/memory-server/`](../memory-server/); this ADR is the short
ledger of *what was decided* and *why this is one decision rather
than ten*.

## Committed design decisions (13)

These are pinned. Changing any of them requires updating
[`docs/memory-server/01-decisions.md`](../memory-server/01-decisions.md)
*first* and propagating the consequences through the rest of
`docs/memory-server/`.

1. **Scope + taxonomy** — both, via M:N join tables.
   `memory_entity_scope` (user/agent/session/project) +
   `memory_entity_tier` (working/episodic/semantic/procedural/
   reflective with fuzzy weights).
2. **Per-client protocol customization** — MCP wire unchanged;
   `OutputFormat` per client (Markdown for Claude Code, CompactJson
   for Codex, Text for generic); tool-description overrides via
   `assets/client_profiles.toml`; default-compact / opt-in-expand
   output handles.
3. **Bi-temporal — everything.** Entities, observations, and
   relations all carry `valid_from`, `valid_to`, `superseded_by`.
4. **LLM extractor — Qwen3-8B-Instruct Q4_K_M via candle.** Local,
   fits 8 GB VRAM alongside BGE-M3. Qwen3-4B fallback; optional
   cloud Haiku 4.5 behind config (off by default).
5. **Embeddings first.** BGE-M3 + Matryoshka migration ships
   *before* any `memory_*` table is created so the new tables ship
   1024d from day one.
6. **Reflection — both modes.** Agent-driven via `memory_reflect`
   MCP tool; cron-driven via `memory-reflect` job (default off).
7. **Forgetting — soft-delete + retention; hard-delete behind
   `cascade=true`.** Default = `valid_to=NOW()`; retention cron
   hard-deletes past the window. Full audit log in
   `memory_forget_log`.
8. **Multi-agent shared memory — first-class.** `memory_scope.agent_id`
   is part of the scope tuple; insert-only writes + Postgres
   transaction isolation handle race conditions.
9. **Code-graph ↔ memory-graph cross-linking — `memory_code_anchor`.**
   Entity anchors to file/chunk/topic with typed `anchor_type`.
   Bidirectional tools.
10. **Evaluation — internal harness primary.** `pgmcp-testing/tests/memory_eval.rs`
    with ≥ 20 scenarios; LongMemEval/LoCoMo as secondary direction
    tracking.
11. **Token efficiency is a first-class design goal** across both
    surfaces (MCP wire + internal LLM pipeline). Concrete targets in
    [`docs/memory-server/02-phases.md`](../memory-server/02-phases.md)
    cross-phase concerns; ≥ 30 % reduction on external tool calls;
    ≥ 30 % reduction + ≥ 1.5× speedup on internal extract→reflect.
12. **Internal latent-space pipeline (RecursiveMAS-style).** Adopt
    inner-RecursiveLink (arXiv:2604.25917) for same-backbone
    pipeline stages, gated on hardware capacity at runtime. Sized
    for RTX 4060 Ti 8 GB; cloud-burst training documented.
13. **Graph-enhanced RAG, selectively.** Adopt NodeRAG heterogeneous
    nodes (arXiv:2504.11544), PathRAG paths (arXiv:2502.14902), and
    empirical gating per GraphRAG-Bench (arXiv:2506.02404). Reject
    LightRAG, LazyGraphRAG, G-Retriever, KAG, and full Microsoft
    GraphRAG with reasons in
    [`docs/memory-server/10-alternatives.md`](../memory-server/10-alternatives.md).

## Rationale

### Why a memory server at all

pgmcp's MTEB-style search and `claude` synthetic project give it
most of the storage and retrieval primitives a memory server needs,
but the user-fact channel today is rule-based (regex over prompts)
and per-session. SOTA memory systems (mem0, Letta, Graphiti) all
have an entity/relation graph with LLM-driven writes; pgmcp's
moat — code-graph integration — would be *amplified* by adding this
layer, not threatened by it.

### Why additive, not a rewrite

The existing `file_chunks` + HNSW + topic clustering + cron jobs +
session-mandate pipeline are working; the memory server slots in
beside them via the `Embedder` / `LlmExtractor` / `Reranker` /
`LatentPipeline` traits (pattern from `src/fcm/mod.rs:144-169`).
Cross-linking happens at `memory_code_anchor` (Phase 2) and the
unified-graph view (Phase 6.3).

### Why hardware-sized choices

The user's GPU is an RTX 4060 Ti with 8 GiB VRAM. Every model
choice — BGE-M3 (1.2 GB) embedder; Qwen3-8B Q4 (5.3 GB) extractor;
BGE-reranker-v2-m3 (0.6 GB) reranker; Qwen3-4B fallback — is sized
to fit that ceiling with mutually-exclusive loading. See
[`docs/memory-server/04-hardware.md`](../memory-server/04-hardware.md).

### Why empirical gating on graph retrieval

GraphRAG-Bench (arXiv:2506.02404) shows graph-enhanced RAG often
*underperforms* vanilla RAG (−13.4% on Natural Questions; +4.5% on
HotpotQA at 2.3× latency). Phase 6.5 enforces A/B testing against
vanilla and auto-disables variants that fail to earn their cost.

## Consequences

### Positive

- pgmcp becomes a drop-in replacement for
  `@modelcontextprotocol/server-memory` for agents that target it
  (Phase 3 official-compat tools).
- Combined code-index + memory-graph queries (e.g., "what file
  implements the auth refactor we discussed?") become possible
  via `memory_code_anchor` cross-linking — a capability no other
  memory server in the field offers.
- LLM-driven salience replaces the regex pipeline (Phase 4),
  capturing nuance the regex misses.

### Negative

- VRAM is tight: Phase 11 RecursiveLink training peaks at ~7 GB on
  the 8 GB ceiling. Mitigated by gradient checkpointing + a
  documented cloud-burst alternative.
- Database storage grows ~3× on embedding columns when migrating
  384d → 1024d (mitigated by Matryoshka truncation at query time).
- Graph-enhanced retrieval tools could underperform vanilla RAG on
  some workloads (mitigated by Phase 6.5 empirical gating).

### Operational

- All new cron jobs default to `enabled = false` except
  `memory-retention` (which only hard-deletes already-soft-deleted,
  low-importance, past-window rows). Opt-in semantics across the
  board.
- `scripts/verify.sh` continues to be the contract; every phase
  ends with the gate green and integration tests added in
  `pgmcp-testing/`.

## When to reconsider

- If a future GPU upgrade lifts the VRAM ceiling, the
  `*BackendChoice` enums + config flips let larger models slot in
  without schema changes (Qwen3-32B extractor; Qwen3-Embedding-8B;
  resident reranker simultaneously).
- If pgmcp is ever deployed as a many-user SaaS, the
  LazyGraphRAG-style deferred summarization pattern becomes
  attractive (currently rejected because the user's single-tenant
  use is high-query-volume).
- If the user's prompt distribution shifts substantially over time,
  the Phase 11 RecursiveLink weights need re-training (cron exists,
  off by default).

## Full design

[`docs/memory-server/`](../memory-server/) — 11 files covering
context, decisions, phases, architecture, hardware, schema, tools,
risks/verification, configuration, milestones, and alternatives
considered.
