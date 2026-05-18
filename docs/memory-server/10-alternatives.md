# 10 — Alternatives considered

Why this plan looks the way it does. Each entry below was evaluated
and either partially adopted or rejected with explicit reasons. The
rationale is captured so future reviewers can reconstruct the design
decisions without re-doing the literature search.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §19.

---

## 10.1 Latent-space inter-agent communication (RecursiveMAS) — *partially adopted*

**Paper:** *Recursive Multi-Agent Systems* (Yang et al.,
arXiv:2604.25917, April 2026). Proposes **RecursiveLink** — a 2-layer
residual projection that transports last-layer hidden states between
LLM agents, eliminating text decode/re-encode between collaboration
rounds. Reports +8.3% accuracy, 1.2–2.4× speedup, 34.6–75.6% token
reduction vs. text-mediated multi-agent baselines on
math/code/search/medical benchmarks.

### External (MCP wire) — not adoptable

1. **MCP is a text/JSON wire.** Tools return strings; there is no
   path to ship raw hidden-state tensors to Claude Code or Codex
   over the standard MCP transport. A custom transport defeats
   decision 3.1 (drop-in compatibility with
   `@modelcontextprotocol/server-memory`) and forces every consuming
   client to learn a new protocol.
2. **Latent states are model-specific.** A Claude-Opus-4.7 hidden
   state has no meaning to Codex; cross-model RecursiveLink would
   need training per (source, target) backbone pair. Anthropic and
   OpenAI models do not expose internal hidden states externally.
3. **Storage cost.** A token's hidden state is ~5–8 KB (d_h × 4
   bytes); a typical observation is dozens of tokens × hundreds of
   bytes for text + 4 KB for the embedding. Storing latents instead
   of text+embedding is 10–50× heavier.

### What's adopted external-facing

The token-budget mindset. The handle-pattern default in Phase 10
(compact handles, opt-in expansion) is the MCP-compatible analogue
of "don't re-tokenize the same content twice across the system."

### Internal pipeline — adopted as Phase 11

For pgmcp's *own* LLM pipeline (extract → reflect → consolidate, all
on the same Qwen3-8B backbone), the inner-RecursiveLink is
hardware-feasible and worth the engineering. Plan in
[`02-phases.md`](02-phases.md) Phase 11; hardware budget in
[`04-hardware.md`](04-hardware.md); risks in
[`07-risks-and-verification.md`](07-risks-and-verification.md);
config in [`08-configuration.md`](08-configuration.md). Training
requirement is acknowledged as a one-shot cost (local ~3–6 hours on
the user's RTX 4060 Ti with gradient checkpointing, or a documented
cloud-burst alternative under $5).

---

## 10.2 GraphRAG-style community summaries (Microsoft full GraphRAG)

**Paper:** *From Local to Global: A Graph RAG Approach to
Query-Focused Summarization* (Microsoft, arXiv:2404.16130). Builds
entity graphs + Leiden community detection + per-community LLM
summaries; excels on "global" thematic queries.

**Why not adopted (yet):** Phase 6 takes RAPTOR + HippoRAG PPR +
NodeRAG heterogeneous view + PathRAG paths, which together cover
hierarchical and multi-hop retrieval at lower indexing cost.
GraphRAG's community-summary build is expensive (an LLM summary per
Leiden community per scope per refresh interval). The plan
**partially uses** the GraphRAG insight — `code_topics` is already a
clustering + c-TF-IDF labeling system that fills a similar role for
the code-graph. Re-applying it to the memory graph is plausible
Phase 6b work once we have evaluation data showing topic-style
queries underserve.

**Critical caveat:** GraphRAG-Bench (arXiv:2506.02404) reports that
full GraphRAG underperforms vanilla RAG on Natural Questions
(−13.4%) and time-sensitive queries (−16.6%); the +4.5% on HotpotQA
comes at 2.3× latency. Phase 6.5 hard-codes empirical gating around
any graph-enhanced retrieval precisely to avoid these failure modes.

---

## 10.3 ColBERTv2 late-interaction retrieval

**Paper:** Santhanam et al., arXiv:2112.01488. Token-level
late-interaction reranking; strong on long-document retrieval.

**Why not adopted (yet):** the chosen BGE-M3 already supports
multi-vector (ColBERT-style) outputs; we just don't use that mode in
Phase 1 (dense only). Switching to multi-vector requires 10–100× the
embedding storage and a different index path. Defer until evaluation
shows reranker (Phase 7) is insufficient.

---

## 10.4 Letta-style core memory blocks (in-prompt)

**Paper:** Packer et al., MemGPT, arXiv:2310.08560; Letta runtime.

**Why not adopted:** pgmcp's memory lives in the DB, not in the
prompt. The closest analogue is the existing `additional_context`
2 KB block injected per prompt (see
[`00-context-and-gap.md`](00-context-and-gap.md) §5.3) — which is
already a *core-memory-block-style* surface and we don't need to
rebuild it. What Letta does that pgmcp doesn't is let the agent
*edit* its core block via tool calls; the `promote_session_mandate`
flow is the nearest equivalent and remains the agent-driven write
path.

---

## 10.5 mem0-style LLM-as-salience-judge with no schema

**Paper:** Mem0, arXiv:2504.19413.

**Why not adopted as the only mode:** mem0's schemaless storage
(everything is a "memory" string with metadata) trades structure for
ease of write. pgmcp Phase 4 uses LLM-extracted *typed* facts
(entity / relation / observation) because the downstream tools
(`memory_relations_traverse`, `memory_facts_at`, code-anchor
cross-graph queries) need typed structure. The LLM-as-salience-judge
idea is adopted — Phase 4's extractor is LLM-driven — but routed
into typed slots, not free text.

---

## 10.6 LightRAG — dual-level retrieval

**Paper:** Guo et al., arXiv:2410.05779 (EMNLP 2025);
[HKUDS/LightRAG repo](https://github.com/HKUDS/LightRAG). Dual-level
retrieval (low-level entity specifics + high-level topic themes)
over an LLM-extracted entity/relation graph, with an
incremental-update algorithm that avoids full re-indexing. ~70–90%
of GraphRAG's answer quality at ~1/100th the indexing cost.

**Why not adopted:** Phase 1 + Phase 2 already supports incremental
updates (file_chunks already cron-driven; memory_* incremental on
every observation). The dual-level idea maps onto Phase 6.1 (RAPTOR
levels) + Phase 6.3 (heterogeneous unified view), which together
provide the same low/high decomposition with explicit abstraction
control via the `levels` parameter on `memory_raptor_search` and
`node_types` on `memory_unified_search`. LightRAG is a strong
reference for what to compare pgmcp against in the Phase 9 eval, not
a separate implementation target.

---

## 10.7 LazyGraphRAG — deferred community summaries

**Reference:** Microsoft Research blog (Nov 2024), no standalone
arXiv. Skips upfront community summarization; uses lightweight
noun-phrase + relationship extraction at query time. Indexing cost
matches vector RAG (~0.1% of full GraphRAG).

**Why not adopted:** the optimization is highest-value when ingest
volume is large and query mix is unknown in advance. pgmcp's primary
user pattern is high session/query volume against a relatively
stable corpus, so eager indexing earns its keep. The LazyGraphRAG
pattern would be the right call if pgmcp were ever deployed as a
many-user SaaS — at that point a `--lazy` mode for the cron jobs
would be a plausible Phase 12. Recorded here for future evaluation.

---

## 10.8 G-Retriever (PCST subgraph extraction)

**Paper:** He et al., NeurIPS 2024, arXiv:2402.07630. Formulates
subgraph retrieval as Prize-Collecting Steiner Tree: maximize node
prizes minus edge costs, returning a connected
context-window-bounded subgraph.

**Why not adopted:** PCST is the right tool for textual knowledge
graphs that vastly exceed the context window. pgmcp's per-scope
graph is small enough that BFS + PathRAG flow-pruning (Phase 6.4) is
sufficient and ~10× simpler to implement. Re-evaluate if
memory-graph size in a single scope ever exceeds ~10⁵ nodes.

---

## 10.9 KAG — Knowledge-Augmented Generation

**Paper:** Liang et al., arXiv:2409.13731;
[OpenSPG/openspg](https://github.com/OpenSPG/openspg). Mutual
indexing between KG and original chunks; logical-form-guided hybrid
reasoning; knowledge alignment. +19.6–33.4% F1 over SOTA on
professional-domain Q&A (e-government, e-health).

**Why not adopted:** KAG presumes a hand-curated domain ontology
(legal codes, medical taxonomies, etc.). pgmcp's domain — user
preferences + project history + code — has no such ontology and
LLM-extracted typed facts (Phase 4) is the closest applicable
substitute. The mutual-indexing idea (KG ↔ chunks) is partially
adopted via `memory_code_anchor` (entity ↔ chunk), which is the
same shape at smaller scope.

---

## 10.10 GraphRAG-Bench — the empirical reality check

**Paper:** Han et al., arXiv:2506.02404 (ICLR'26);
[GraphRAG-Bench repo](https://github.com/GraphRAG-Bench/GraphRAG-Benchmark).
First head-to-head benchmark across GraphRAG, LightRAG, NodeRAG,
HippoRAG, vanilla RAG. Findings used as input to Phase 6.5 gating
discipline.

**Adopted as design pressure, not as a system.** Phase 6.5 cites
this paper explicitly. Phase 9's eval harness is structured to
surface the same trade-offs the benchmark measures (recall ×
latency × token cost), so when an implementation lands we can claim
"X tool helps on multi-hop; Y tool regresses vs vanilla on lookups"
with our own numbers rather than vendor claims.

---

## See also

- [`01-decisions.md`](01-decisions.md) — the 13 committed decisions
  that reflect these adopt/reject choices.
- [`02-phases.md`](02-phases.md) — Phase 6.3 (NodeRAG), 6.4
  (PathRAG), 6.5 (GraphRAG-Bench discipline), Phase 11 (RecursiveMAS
  internal) — the phases that implement the partially-adopted
  ideas.
- VentureBeat overview (rate-limited at the time of survey):
  <https://venturebeat.com/orchestration/architectural-patterns-for-graph-enhanced-rag-moving-beyond-vector-search-in-production>
  — referenced for industry framing.
