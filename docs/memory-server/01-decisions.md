# 01 — Committed design decisions

This document is the canonical decisions ledger for the memory-server
build-out. Every downstream phase ([`02-phases.md`](02-phases.md)),
schema choice ([`05-schema.md`](05-schema.md)), and tool signature
([`06-tools.md`](06-tools.md)) implements these commitments.

If a decision needs to change, edit this file *first*, then propagate
the consequences through the rest of the docs.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §9 plus
> the cross-cutting commitments from §1.

---

## The 10 core decisions

The original exploration raised ten open questions; all are now
resolved.

### 1. Scope + taxonomy — both, via join tables

- `memory_scope` carries (`user_id`, `agent_id`, `session_id`,
  `project_id`); `memory_entity_scope` is M:N so an entity can be
  shared across scopes without row duplication.
- `memory_entity_tier` is M:N with fuzzy weights; an entity can be
  primarily semantic while also episodic.
- Rationale: aligns with semantic search (filtering happens via
  `JOIN` not column-OR), and clean orthogonality between *who owns
  it* and *what kind of memory it is*.

See [`05-schema.md`](05-schema.md) for the SQL.

### 2. Per-client protocol customization

- MCP wire format unchanged.
- `OutputFormat` chosen by client: Claude Code → Markdown, Codex →
  CompactJson, generic → Text.
- Per-(client, tool) description overrides in
  `assets/client_profiles.toml`.
- Optional `brief`/verbosity hints on all tools.
- Default-compact / opt-in-expand outputs (handle pattern).
- See Phase 10 in [`02-phases.md`](02-phases.md).

### 3. Bi-temporal — everything

- Entities, observations, and relations carry `valid_from`,
  `valid_to`, `superseded_by`.
- Default queries filter `WHERE valid_to IS NULL`.
- `memory_facts_at(t)` exposes point-in-time view.
- Eviction (Phase 8) defaults to soft-delete = `valid_to=NOW()` for
  free.

### 4. LLM extractor — Qwen3-8B-Instruct Q4_K_M via candle

- Local, fits in 8 GB VRAM alongside BGE-M3.
- Fallback `Qwen3-4B-Instruct` for tight-VRAM configurations.
- Optional cloud Haiku 4.5 path behind config (off by default).
- See Phase 4 in [`02-phases.md`](02-phases.md) and the VRAM budget
  in [`04-hardware.md`](04-hardware.md).

### 5. Embeddings first

Phase 1 lands BGE-M3 + Matryoshka migration before any `memory_*`
table is created. The memory tables ship with 1024d from day one.

### 6. Reflection — both modes

- Agent-driven: `memory_reflect` MCP tool.
- Cron: `memory-reflect` job, configurable interval, default off.
- Both call the same reflection prompt path through the
  `LlmExtractor` trait.

### 7. Forgetting — soft-delete + retention; hard-delete behind cascade

- Default: `valid_to=NOW()`, row remains queryable via
  `memory_facts_at(t)`.
- Cron `memory-retention`: hard-deletes rows older than `window_days`
  (default 90), low importance, no superseded chain remaining.
- `memory_forget(... cascade=true)` for explicit
  right-to-be-forgotten with full audit log in `memory_forget_log`.

### 8. Multi-agent shared memory — first-class scope dimension

- `memory_scope.agent_id` is part of the scope tuple.
- Sharing semantics: an entity referenced by multiple scopes (M:N via
  `memory_entity_scope`) is visible to each.
- Race-condition discipline: insert-only writes; bi-temporal
  invalidation never destroys; Postgres transaction isolation does
  the rest.

### 9. Code-graph ↔ memory-graph cross-linking — `memory_code_anchor`

- Anchors an entity to a `file_id`, `chunk_id`, or `topic_id` with a
  typed `anchor_type` (e.g. `implements`, `tested-by`,
  `documented-in`, `caused-by`, `applies-to`).
- Bidirectional tools: `memory_find_code_for_entity`,
  `memory_find_entities_for_code`.
- This is the pgmcp moat (see
  [`00-context-and-gap.md`](00-context-and-gap.md) §6 takeaway 7).

### 10. Evaluation — internal harness primary

- `pgmcp-testing/tests/memory_eval.rs` defines ≥ 20 scenarios.
- Cron `memory-eval` (default off) scores periodically.
- LongMemEval / LoCoMo subsets present as secondary direction
  tracking, not absolute scores.

---

## Three cross-cutting commitments

Raised after the initial 10 decisions were settled.

### 11. Token efficiency is a first-class design goal

Applies to both surfaces:

- *External (MCP wire to clients).* Tool outputs default to handles;
  verbose content opt-in via `expand=true`. Target: ≥ 30 % reduction
  in output tokens per typical tool call vs a naïve "return
  everything as Markdown" baseline, measured by the Prometheus
  counter `pgmcp_tool_response_tokens` (already in pgmcp's telemetry
  as of commit 8011a2b). See Phase 10.
- *Internal (pgmcp's own LLM pipeline).* Same-backbone pipeline
  stages skip text round-trips via RecursiveLink (Phase 11) when
  hardware permits. Target on the user's RTX 4060 Ti: replicate the
  RecursiveMAS paper's order-of-magnitude — ≥ 30 % token reduction
  and ≥ 1.5× speedup on the extract → reflect path, with no
  extraction-quality regression (validated by Phase 11.4).
- Both targets are recorded in `pgmcp_metadata` and surfaced via
  `/api/memory/pipeline_stats` and `pgmcp context`.

### 12. Internal latent-space pipeline (RecursiveMAS-style)

Adopt RecursiveMAS (arXiv:2604.25917) inner-RecursiveLink hidden-state
hand-off between same-backbone pipeline stages, **gated on hardware
capacity** at runtime.

- Phase 11.
- Sized for the user's RTX 4060 Ti 8 GB.
- Local training of the RecursiveLink weights is supported with
  gradient checkpointing (~7 GB peak); a cloud-burst path is
  documented for the one-shot training step if local training proves
  marginal.
- Inference is local-only.
- See Phase 11 in [`02-phases.md`](02-phases.md) and the VRAM table in
  [`04-hardware.md`](04-hardware.md).

### 13. Graph-enhanced RAG, selectively

Adopt:

- **NodeRAG** heterogeneous-node graph view (arXiv:2504.11544, Phase
  6.3).
- **PathRAG** path-based retrieval with flow-pruning (arXiv:2502.14902,
  Phase 6.4).
- **Empirical gating** per GraphRAG-Bench (arXiv:2506.02404, Phase
  6.5) — all graph-enhanced tools are opt-in at the call site;
  vanilla `memory_semantic_search` remains the default retrieval
  path.

Reject (with reasons documented in [`10-alternatives.md`](10-alternatives.md)):

- **LightRAG** — duplicates RAPTOR + heterogeneous view we already
  plan.
- **LazyGraphRAG** — pgmcp's eager indexing earns its keep for this
  user.
- **G-Retriever** PCST — overkill for pgmcp's graph size.
- **KAG** — requires a curated ontology pgmcp doesn't have.
- **Full Microsoft GraphRAG** — community-summary build is exactly
  the cost profile GraphRAG-Bench cautions against.

---

## See also

- [`02-phases.md`](02-phases.md) — phase-by-phase implementation plan
  that implements every decision above.
- [`05-schema.md`](05-schema.md) — SQL that implements decisions 1,
  3, 8, 9.
- [`06-tools.md`](06-tools.md) — MCP tools that implement decisions
  2, 6, 7, 13.
- [`08-configuration.md`](08-configuration.md) — TOML keys that
  control decisions 7, 10, 11, 12, 13.
- [`10-alternatives.md`](10-alternatives.md) — full reasoning for the
  systems rejected in decision 13.
- `docs/decisions/002-sota-memory-server-design.md` — short ADR that
  pins this file as the canonical decision record.
