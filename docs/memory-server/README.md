# pgmcp Memory Server — Design Documents

This directory holds the canonical design for extending pgmcp into a
**state-of-the-art memory server** for LLM agents (Claude Code,
Codex, and any MCP client) — *in addition to* pgmcp's existing
code-indexing, semantic-search, and graph-analytics surface.

The memory server is a **new layer** on pgmcp, not a rewrite. Every
existing tool, table, and cron job stays untouched throughout the
build-out.

> The working planning artifact lives at
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` (outside
> the repo). This directory is the canonical, committed mirror —
> once a phase ships, the in-repo docs become the source of truth,
> the plan file reduces to a "what's next" pointer, and the
> as-built record lives in
> [`09-milestones-and-as-built.md`](09-milestones-and-as-built.md).

---

## Quick orientation

If you have five minutes, read these three files first:

1. [`00-context-and-gap.md`](00-context-and-gap.md) — what a memory
   server is, what's SOTA in the field, and where pgmcp is today
   (§5 audit) vs. where the gap is (§6 matrix).
2. [`01-decisions.md`](01-decisions.md) — the 13 committed design
   decisions that pin down the rest of the docs.
3. [`02-phases.md`](02-phases.md) — Phase 0 (quick wins) through
   Phase 11 (RecursiveMAS-style latent pipeline).

If you have an hour, read the rest in order.

---

## Document map

| File | Contents | Cross-references |
|---|---|---|
| [`00-context-and-gap.md`](00-context-and-gap.md) | Context · what is a memory server · existing landscape · SOTA across 9 axes · pgmcp current state · gap analysis matrix | — |
| [`01-decisions.md`](01-decisions.md) | 13 committed design decisions (scope+taxonomy, bi-temporal, multi-agent, code-anchor, etc.) | All other docs implement these |
| [`02-phases.md`](02-phases.md) | Phase 0 → Phase 11 implementation plan with cross-phase concerns | Schema in `05`, tools in `06`, traits in `03` |
| [`03-architecture.md`](03-architecture.md) | Topology diagram + Rust trait surfaces (`Embedder`, `LlmExtractor`, `Reranker`, `LatentPipeline`, `GpuDispatcher`) | Phases 1, 4, 7, 11 |
| [`04-hardware.md`](04-hardware.md) | RTX 4060 Ti 8 GB VRAM budget + model choices justified | Phases 1, 4, 7, 11 |
| [`05-schema.md`](05-schema.md) | Full SQL for Phase 1 (embedding columns), Phase 2 (memory tables), Phase 6.3 (unified-graph views) | Phase 2, 6.3 |
| [`06-tools.md`](06-tools.md) | MCP tool catalog grouped by phase, with signatures | Phase 0, 3, 5, 6, 8, 10 |
| [`07-risks-and-verification.md`](07-risks-and-verification.md) | Risk register + per-phase test surface | All phases |
| [`08-configuration.md`](08-configuration.md) | TOML config keys (`[memory.*]`, `[cron]` additions) | All phases |
| [`09-milestones-and-as-built.md`](09-milestones-and-as-built.md) | M1–M7 milestones + append-only shipped log | Implementation tracking |
| [`10-alternatives.md`](10-alternatives.md) | RecursiveMAS, GraphRAG, ColBERTv2, Letta, mem0, LightRAG, LazyGraphRAG, G-Retriever, KAG, GraphRAG-Bench — adopt/reject reasoning | Phase 6.3, 6.4, 6.5, 10, 11 |

Plus the short ADR
[`docs/decisions/002-sota-memory-server-design.md`](../decisions/002-sota-memory-server-design.md)
that lists the 13 commitments and points back to this directory.

---

## What this changes about pgmcp

Three concrete shifts, all additive:

1. **New `memory_*` MCP tool surface** that's drop-in compatible with
   `@modelcontextprotocol/server-memory` (entities + relations +
   observations CRUD), plus pgmcp extensions for semantic search,
   bi-temporal point-in-time queries, code-graph cross-linking,
   reflection, hierarchical retrieval, and forgetting.
2. **New `memory_*` tables** behind those tools (entities,
   observations, relations, scope, tier joins, code anchor, RAPTOR
   summary tree, forget log, reflection runs). All bi-temporal by
   default — `valid_from`, `valid_to`, `superseded_by` on every
   fact.
3. **New compute backends** — BGE-M3 embedder (replacing
   MiniLM-L6-v2), Qwen3-8B Q4 local LLM extractor (replacing the
   regex salience pipeline), BGE-reranker-v2-m3 cross-encoder
   reranker, optional RecursiveLink latent-space pipeline (Phase 11,
   hardware-gated).

None of pgmcp's existing capabilities go away. The code-indexing
graph (`code_graph_edges`), the topic clustering (`code_topics`),
the durable-mandate persistence, the cross-project similarity
table, and the `claude` synthetic project all stay live and
unchanged. The memory server cross-links *into* them via
`memory_code_anchor` (Phase 2) and the unified graph view (Phase
6.3).

---

## Doc maintenance discipline

This directory is **as-built documentation**, not aspirational. Every
implementation PR carries the relevant doc updates in the same
change:

| Change in source | Required doc update |
|---|---|
| New SQL migration | [`05-schema.md`](05-schema.md) reflects the *as-built* DDL |
| New MCP tool | [`06-tools.md`](06-tools.md) lists the exact signature + a usage example |
| New trait or backend | [`03-architecture.md`](03-architecture.md) lists the trait + factory + default impl |
| New TOML key | [`08-configuration.md`](08-configuration.md) is the canonical reference |
| Milestone landing | [`09-milestones-and-as-built.md`](09-milestones-and-as-built.md) gets a new "Shipped" entry with date, commit range, deviations, eval scorecard |
| Design-decision change | [`01-decisions.md`](01-decisions.md) is updated *first*, then consequences propagated |
| Alternative re-evaluated | [`10-alternatives.md`](10-alternatives.md) updated with new reasoning |

For incidents and non-trivial debugging episodes during build-out,
follow the existing pattern in
[`docs/scientific-ledger/`](../scientific-ledger/) (see
`oom-fix-2026-04-22.md` and `recovery-times-2026-04-28.md` for
format precedent). Link to ledger entries from
[`09-milestones-and-as-built.md`](09-milestones-and-as-built.md)
rather than duplicating content.

---

## Status

- **Initial doc seed:** ✅ shipped (this directory exists).
- **M1 — Foundations:** in progress
  ([`09-milestones-and-as-built.md`](09-milestones-and-as-built.md)
  tracks the live state).
- **M2–M7:** pending; no fixed dates, paced by the user.

Implementation work starts from
[`02-phases.md`](02-phases.md) Phase 0.
