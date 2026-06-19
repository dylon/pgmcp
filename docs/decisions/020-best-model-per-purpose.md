# ADR-020: Best model per purpose — the pgmcp ML/LLM model portfolio

- Status: Accepted (evidence-based; revisited per future bake-offs)
- Date: 2026-06-19
- Supersedes/relates: synthesizes the semantic-search evaluation
  (`docs/evaluation/semantic-search-quality.md`, experiments #65 + #106) and its
  four follow-up epics; relates to ADR-004 (BGE-M3 embedding migration), ADR-005
  (removal of the MiniLM-384 path), ADR-002 (SOTA memory-server design, the LLM
  extractor seam), and ADR-017 (topic-clustering redesign). Standing user
  directive: *prefer locally-runnable models where genuinely superior; use the
  best tool per purpose — traditional static analysis, ML/classification, or an
  LLM — not an LLM by default.*

## Context

pgmcp runs **many** learned and algorithmic components: dense embeddings, two
rerankers, RAPTOR/topic clustering, an LLM extractor/judge, and a large suite of
classical graph + statistics analyses. Until this campaign there was no
*evidence-based, written* record of **which model serves which purpose and why** —
the choices were scattered across ADRs and code. The retrieval-quality evaluation
and its follow-ups (Epics 1–4) produced the first head-to-head, rank-metric +
paired-statistics evidence across the retrieval pipeline; this ADR turns that
evidence into a portfolio decision per purpose, names the config seam to change
each one, and states what new evidence would flip it.

**Compute substrate.** The local workstation has an 8 GiB GPU (RTX 4060 Ti) that
the daemon's embedder pool usually occupies. A network-reachable **NVIDIA DGX
Spark (`sparky`, GB10, ~128 GiB unified memory)** is now available for heavy
local inference (see the `reference-sparky-dgx` memory). This makes large *local*
models (DeepSeek-V4, Qwen3) practical for **offline** work (evaluation, judging,
bake-offs) without contending with the live daemon — strengthening the local-first
posture: cloud is opt-in, never required.

## Decision — the portfolio

| purpose | chosen model | locality | evidence | config seam |
|---|---|---|---|---|
| **Dense embeddings** (corpus + query) | **BGE-M3** 1024-d, CLS-pooled, L2-norm | local (GPU/BF16) | recall@10 0.74 known-item; M1 leak-controlled strip-&-re-embed still 50 % R@10; M2 leak-free; HNSW lossless (§5.1–5.4) | `EmbeddingModel` enum, `src/embed/model.rs` |
| **Second-stage rerank** | **ColBERT MaxSim** (BGE-M3 late-interaction head); cross-encoder **off** by default | local (GPU) | F7: ColBERT small lexical lift (paired Wilcoxon `p_adj`=0.003, Cliff's δ 0.13), neutral conceptual; BGE-reranker-v2-m3 cross-encoder *significantly hurts* lexical (δ −0.24) — experiment #106 (§5.7) | `RerankerChoice`, `src/reranker/mod.rs` |
| **Graph-augmented retrieval** | `code_ppr_search` (PPR/HippoRAG) for relational queries; **not** a flat-retrieval replacement | local (DB + graph algos) | F8: every graph mode below flat semantic for single-target recall; `code_ppr` closest, `code_raptor` ≈ 0 at file granularity (§5.8) | tools in `src/mcp/tools/`; artifacts via the graph/RAPTOR crons |
| **Conceptual relevance judge** (offline eval) | **DeepSeek-V4** (`deepseek-v4-pro`) on sparky; cross-family κ vs Qwen3 | local (sparky GPU) | Epic 2 / §5.9 / exp #108: semantic nDCG@10 **0.87 ≫ hybrid 0.72** (δ=−0.60 *large*); cross-family Cohen's **κ=0.81** (vs Qwen3-14B, n=53); deepseek fast+clean, qwen3-32b CPU-offloads on GB10 ollama, qwen3-14b serviceable | `pgmcp-testing/src/eval/judge.rs`, `PGMCP_JUDGE_*` env |
| **Salience extraction / reflection / summaries** (in-daemon) | **Qwen3-4B** local (Q4_K_M, candle); **cloud Haiku** opt-in | local (GPU) / cloud | `LlmExtractor` trait + factory; trust-boundary-bounded (ADR-002) | `LlmBackendChoice`, `src/llm/mod.rs` |
| **Topic / module clustering** | **Louvain over `code_graph_edges`** (graph), not an LLM | local (algorithmic) | ADR-017 bake-off: graph engine the default winner; LLM labels are an *optional* post-step | `src/topic_analysis/`, ADR-017 |
| **Static / structural analyses** (secrets, taint, centrality, deadlock, …) | **classical static analysis + graph algorithms**, never an LLM | local (algorithmic) | deterministic, auditable, fast; LLMs add cost + nondeterminism for no gain | the respective `src/mcp/tools/` + `src/graph/` |
| **Significance testing** (this campaign) | **paired Wilcoxon + Cliff's δ + bootstrap + BH-FDR**, never an LLM | local (algorithmic) | `src/stats/inference.rs`; an LLM "vibe check" cannot replace a pre-registered test | `src/stats/`, the experiment subsystem |

## Rationale per purpose

### Embeddings — keep BGE-M3
BGE-M3 is multilingual, handles code + prose + transcripts, and is the only model
the corpus is currently embedded with (1024-d `embedding_v2`). The evaluation
shows it is *fit for purpose* (§5.1–5.4) and that its semantics are genuine, not
identifier echo (M1 strip-and-re-embed retains 50 % recall@10; M3 redaction barely
moves it). The one known weakness is the **512-token window** (≈ 23 % of chunks
exceed it; §5.5) — addressed by sub-chunking, *not* by swapping the model.

*What would change this:* a full alternative-embedder bake-off (e.g. a newer
code-specialized local embedder) measured on the same harness (known-item + M1 +
M2). This is **deliberately scoped as a future experiment**, not run inline,
because a fair comparison requires re-embedding the entire 644 k-chunk corpus with
the candidate (a multi-hour GPU job + a new `EmbeddingModel` variant) — the harness
(`eval-retrieval`) and the leak-controlled M1 stratum are ready to drive it on
sparky when a candidate is chosen. The *scoring*-model bake-off (dense vs ColBERT
vs cross-encoder) **was** run (Epic 1) and is decisive on its own.

### Reranking — ColBERT safe, cross-encoder gated off
Epic 1 (experiment #106, §5.7) is the head-to-head. The takeaway is the same
shape as the hybrid-vs-semantic finding (F6): **the right model is
query-distribution-dependent.** ColBERT late-interaction never hurts (small
lexical lift, neutral conceptual) → safe to enable in the rerank hook. The
cross-encoder, trained on natural-language query↔passage pairs, *misjudges*
verbatim-text-vs-code and significantly hurts the M2 lexical stratum → it must not
be a blanket default.

### Graph retrieval — complement, not replacement
Epic 3 (§5.8) confirms the graph tools are for *relational* and *module-level*
questions, not "find the one file." `code_ppr_search` is the strongest (it
restarts PageRank on the dense hits, inheriting recall) but still trails flat
semantic for single-target retrieval; `code_raptor_search` is ≈ 0 at file
granularity because the corpus's RAPTOR clusters are too coarse — a *clustering*
defect (tune HDBSCAN/c-TF-IDF), not an LLM-choice defect.

### Judge — DeepSeek-V4 (sparky), with a cross-family κ
Epic 2 (§5.9) needed a strong, *local* relevance judge. On sparky:
**DeepSeek-V4-pro** is fast (~2.4 s/grade), clean (separates reasoning), and
frontier-quality — the best available local judge. **Qwen3-32B** is, in
principle, the cross-family partner, but ollama on the GB10 **CPU-offloads ~47 %
of a 32B model** (unified-memory VRAM-detection cap), making it unusably slow
(> 300 s/grade); **Qwen3-14B** fits the GPU fully and serves as the cross-family
κ check (its OpenAI-endpoint thinking mode is occasionally unparseable, so κ is
best-effort). **In practice it worked well:** cross-family quadratic Cohen's
κ = **0.81** (almost-perfect, n = 53), and the judge confirmed the headline
routing finding — semantic nDCG@10 **0.87 ≫ hybrid 0.72** on conceptual queries
(δ = −0.60 *large*, F9). Recommendation: use **DeepSeek-V4** as the primary offline
judge and a **Qwen3** model for the cross-family agreement check; record the GB10
32B limitation so a future fix (NIM serving, or `num_gpu` tuning) can restore 32B.
For *in-daemon* LLM work (extraction/reflection), the lighter **Qwen3-4B** local
backend (or opt-in cloud) remains correct — judging is offline and can afford the
big model; the daemon path cannot.

### Don't default to an LLM
Per the standing directive, the portfolio uses the **best tool per purpose**:
graph algorithms for structure/centrality/communities, classical static analysis
for secrets/taint, ML clustering (Louvain/HDBSCAN) for topics, and
pre-registered non-parametric statistics for the evaluation itself. LLMs are used
only where they are genuinely superior (relevance judgement, natural-language
salience extraction, summaries) — and even there, locally.

## Consequences

- A single, queryable record of the model-per-purpose decisions with their config
  seams; future swaps have a documented baseline + a ready harness.
- ColBERT can be enabled as the default rerank in the `/api/search` hook for
  lexical/hybrid queries; the cross-encoder stays gated.
- The 32B-judge limitation on the GB10 is documented; until resolved, DeepSeek-V4
  is the local judge of record for offline eval.
- The alternative-dense-embedder bake-off is an explicit, scoped future experiment
  (corpus re-embed required) — not a silent omission.

## References

- `docs/evaluation/semantic-search-quality.md` — the full evaluation (§5.7 rerank,
  §5.8 graph, §5.9 judge, §6 findings F1–F8).
- `docs/scientific-ledger/reranker-a-b-…-2026-06-19.md` — experiment #106.
- ADR-004 (BGE-M3), ADR-002 (LLM extractor seam), ADR-017 (topic clustering).
- `reference-sparky-dgx` (memory) — the DGX Spark compute substrate.
