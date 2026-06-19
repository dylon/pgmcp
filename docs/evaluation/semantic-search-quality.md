# Evaluation: the Quality of pgmcp's Semantic Search

**Status:** complete (first campaign) · **Date:** 2026-06-17 · **Harness:**
`pgmcp-testing/src/bin/eval_retrieval.rs` (+ `src/quality/retrieval_metrics.rs`,
`pgmcp-testing/src/eval/`) · **Raw results:** `target/eval/retrieval_results_full.json`
· **Experiment ledger:** `docs/scientific-ledger/pgmcp-semantic-search-retrieval-quality-2026-06-17.md`
(pgmcp experiment #65, slug `pgmcp-semantic-search-retrieval-quality`)

---

## 1. TL;DR — the verdict

On a 50-query, intent-phrased **known-item** benchmark over the live 644 K-chunk
corpus, **`semantic_search` is the strongest retrieval mode** and is *fit for
purpose* for conceptual code search:

- It places the gold file in the **top-10 for 74 %** of queries (recall@10 =
  0.74), with **MRR = 0.301** and **nDCG@10 = 0.405**.
- It **significantly beats lexical `text_search`** on every metric (nDCG@10
  Cliff's δ = −0.69, *large*; `p_adj < 10⁻⁴`) — exactly the conceptual-query
  regime where keyword matching fails.
- **`hybrid_search` does *not* help here** — it is *slightly but significantly
  worse* than pure semantic on top-rank quality (nDCG@10 Δ = −0.060, δ = −0.114,
  *negligible*; identical recall@10). Fusing a near-useless lexical leg via RRF
  dilutes a strong semantic ranking on conceptual queries.
- **…but `hybrid` *wins* when the query is lexical.** A leak-free full-corpus
  probe (**M2**, N = 60: queries drawn from text *beyond* the 512-token window)
  is the mirror image — on these verbatim-text queries `hybrid` **significantly
  beats** semantic (nDCG@10 δ = +0.159 *small*, `p_adj = 0.01`). Together these
  pin *when to fuse*: **hybrid for keyword/lexical queries, pure semantic for
  conceptual ones** — exactly what the `bm25_weight`/`semantic_weight` knobs are
  for.

Supporting findings:

- **The HNSW index is exact at this scale**: recall vs a brute-force scan =
  **1.000** at `ef_search ∈ {40, 100, 200}`. The approximation is costing
  nothing; the default `ef_search = 100` is safe (even 40 suffices here).
- **≈ 23 % of chunks exceed the 512-token embedding window** (≈ 147 K of 644 K).
  Their tail is silently dropped from the dense vector (it survives in the
  lexical `content_tsv`). This is the single largest *latent* quality lever.
- **Pattern-catalog crowding is mild** (3.7 % of top-5 slots), not the dominant
  pathology that ad-hoc probing suggested.
- A **leakage-controlled docstring→code benchmark** (strip-and-re-embed; the M1
  control, N = 60) confirms it: with the doc-comment *provably removed* from the
  embedding, semantic search still lands the exact code chunk in the **top-10 for
  ≈ 48 %** of queries (rank-1 for ≈ 23 %) among 461 chunks — and **redacting
  identifiers barely changes that** (recall@10 0.48 → 0.50), proving the match is
  *semantic*, not keyword/identifier echo. (`text` ≈ 0 here — lexical cannot match
  a doc whose words were stripped from the body.)

The headline gap is **conceptual-vs-lexical**, not semantic-vs-hybrid: pgmcp's
dense retrieval is doing its job; the lexical and fused modes are the right tools
for *different* (keyword-precise) queries, which this set deliberately excludes.

**Follow-up campaigns (Epics 1–4, 2026-06-19) — all four implemented, local-model-first:**

- **Reranking (§5.7, F7):** local **ColBERT** MaxSim is a small-but-consistent win
  on lexical M2 (nDCG +0.07, paired `p` = 0.003) and neutral on conceptual → the
  safe default; the BGE cross-encoder *hurts* lexical (nDCG δ = −0.24) → gated off.
- **Graph modes (§5.8, F8):** `code_ppr` / `code_path` / `code_raptor` all trail
  flat semantic for single-target retrieval — they *complement* (relational /
  module queries), not replace, dense search.
- **LLM-as-judge (§5.9, F9):** an independent local judge (**DeepSeek-V4**,
  cross-family Cohen's **κ = 0.81** vs Qwen3-14B) over 40 conceptual queries
  confirms the routing rule with a *large* effect — **semantic nDCG@10 0.87 ≫
  hybrid 0.72** (fusion hurts conceptual).
- **Truncation (§5.5, F4):** the chunk-granularity pass quantifies the 512-token
  cost — semantic recall@10 0.90 (file) → **0.65 (chunk)**, a ≈ 25-point per-chunk
  penalty.
- A **CI regression gate** + opt-in **drift cron** now guard retrieval quality;
  **ADR-020** records the best-model-per-purpose portfolio. (Experiments
  #106 / #107 / #108.)

---

## 2. Why this evaluation exists

Before this campaign, pgmcp had **no measurement of retrieval relevance**. The
benchmarks (`benches/`) timed latency only; the tests asserted *findability* (is
a seeded row present at all) or *RRF-formula* correctness — never recall@k / MRR /
nDCG against labeled relevance. The single `nDCG@10` mention in the repository was
an aspirational row for files that were never built. "Is the search any good?"
was, literally, unanswered.

This evaluation answers it with a **reproducible, statistically-grounded,
rank-based** methodology and a committed harness so the answer can be regenerated
and tracked over time.

---

## 3. The system under test

```
                 ┌──────────────────────────────────────────────────────────┐
   query  ─────► │  BGE-M3 encoder (XLM-RoBERTa-large, 1024-d, CLS-pooled,   │
                 │  L2-normalized)  →  q ∈ ℝ¹⁰²⁴, ‖q‖₂ = 1                   │
                 └───────────────────────────┬──────────────────────────────┘
                                             │  cosine = dot product
                                             ▼
                 ┌──────────────────────────────────────────────────────────┐
   corpus  ────► │  pgvector HNSW index on file_chunks.embedding_v2          │
   644 428       │  vector_cosine_ops · m = 24 · ef_construction = 200 ·      │
   chunks        │  ef_search = 100                                          │
                 └───────────────────────────┬──────────────────────────────┘
                                             │  ORDER BY embedding_v2 <=> q
                                             ▼
                 ┌──────────────────────────────────────────────────────────┐
   results ◄──── │  top-k chunks, score = 1 − cosine_distance ∈ [−1, 1]     │
                 │  (no score floor, no rerank / MMR in semantic_search)     │
                 └──────────────────────────────────────────────────────────┘
```

| Property | Value | Source |
|---|---|---|
| Embedding model | **BGE-M3** (`BAAI/bge-m3`), 1024-d, CLS-pooled, L2-normalized | `src/embed/model.rs` |
| Precision | BF16 on CUDA (corpus), F32 on CPU (this campaign's queries) | `src/embed/model.rs` |
| Index | pgvector **HNSW**, cosine (`vector_cosine_ops`) | `src/db/migrations.rs` |
| Index params | `m = 24`, `ef_construction = 200`, `ef_search = 100` | `src/config.rs` |
| Score | `score = 1 − (embedding_v2 <=> q)` = cosine similarity | `src/db/queries/search.rs` |
| Rerank | none in `semantic_search` (MMR / ColBERT / cross-encoder live in `/api/search`) | `src/mcp/tools/tool_semantic_search.rs` |

Three retrieval modes are compared, all chunk-granularity and ready against the
live corpus:

- **`semantic_search`** — pure HNSW cosine over the dense BGE-M3 vector.
- **`text_search`** — Postgres full-text (`plainto_tsquery` × `content_tsv`,
  ranked by `ts_rank`). Lexical/keyword precision.
- **`hybrid_search`** — **Reciprocal Rank Fusion** of the text + semantic legs
  (+ an optional per-project WFST leg, dormant here), `score = Σ wᵢ/(k+rankᵢ)`,
  `k = 60`, default weights `0.5/0.5` (Cormack, Clarke & Büttcher 2009).

The graph-augmented `code_*` tools (PPR, PathRAG, RAPTOR) are **out of scope**
for this campaign — their artifact tables were unpopulated at run time — and are
a documented follow-up (§9).

---

## 4. Methodology

### 4.1 Why rank-based metrics, not a similarity threshold

BGE-M3 cosine similarities on this corpus occupy a **tight ≈ 0.56–0.68 band**
(high-dimensional distance concentration): an off-topic query still returns its
ten *least-bad* chunks with deceptively high-looking scores, and the fused RRF
scores (≈ 0.008) are not comparable to cosine at all. **Ranking is the signal.**
Every metric below is derived from ranks; absolute score appears only as a
brittleness diagnostic. This is the standard stance for cross-system IR
comparison.

### 4.2 Ground truth (no labels existed)

Two **objective** strategies, triangulated. There is no LLM-as-judge in this
campaign (a documented follow-up, §9).

**(A) Known-item — the human anchor (N = 50, live corpus).** Hand-authored
natural-language queries phrased in *intent* language, each with a single gold
file verified to exist (`pgmcp-testing/src/eval/query.rs`). Queries deliberately
avoid the target's compound identifiers, so they exercise semantic recall, not
filename echo — but they intentionally *mix* lexical-friendly phrasings (where
text/hybrid can win) with purely conceptual ones (where semantic should win), so
the comparison is not rigged toward either mode. A few adversarial entries name a
concept whose tempting wrong answer is a `src/patterns/*` catalog card, to probe
top-k crowding. Leak-free by construction: the queries are written by a human, not
copied from the code.

**(B) Docstring-as-query, leakage-controlled — the scalable objective set
(CodeSearchNet-style; Husain et al. 2019).** A code chunk's leading doc-comment
becomes the query and the chunk is the gold target. The catch is **leakage**: if
the doc-comment sits inside the embedded chunk, retrieval is trivial. The **M1**
control severs it exactly — in an isolated throwaway database, each target is
**re-embedded with its doc-comment removed** (`pgmcp-testing/src/eval/corpus.rs`),
so the stored vector never saw the query, surrounded by distractor chunks embedded
in full. The **M3** variant additionally redacts identifier tokens from query and
body, isolating real semantics from identifier echo.

### 4.3 Metrics (formulae)

Per query `q`, ranked list `r₁, r₂, …` after **path-dedup**, gold set `G_q`,
graded gain `rel(d)`, `k ∈ {1, 5, 10, 20}` (`src/quality/retrieval_metrics.rs`):

- **Success@k** `= 𝟙[ top-k ∩ G_q ≠ ∅ ]`
- **recall@k** `= |top-k ∩ G_q| / |G_q|`
- **precision@k** `= |top-k ∩ G_q| / k`
- **MRR** `= mean( 1 / rank_of_first_relevant )`, 0 if absent — *primary*
- **AP** `= (1/|G_q|) Σ_{k: rₖ∈G_q} precision@k`;  **MAP** `= mean(AP)`
- **DCG@k** `= Σᵢ rel(rᵢ) / log₂(i+1)`;  **nDCG@k** `= DCG@k / IDCG@k ∈ [0,1]` — *primary*

`hybrid_search` emits the same path once per fused leg; **all modes are
path-deduped (first occurrence) before scoring**, so duplicates never inflate a
metric. Cross-mode comparison is at **file granularity** (the uniform key, since
`semantic_search` rows carry no chunk id), which also makes a future
file-granularity graph tool directly comparable.

### 4.4 Statistics

The unit of analysis is the **query**; per-mode metric vectors are aligned by
query id. For every unordered pair of modes (`src/stats/inference.rs`):

- **Wilcoxon signed-rank** on the paired per-query values (non-parametric, robust
  to the bounded/tied/bimodal IR distribution) — Wilcoxon (1945).
- **Cliff's δ** effect size + **bootstrap CI** (BCa, seeded) on the mean
  difference — Cliff (1993); Efron (1987).
- **Benjamini-Hochberg FDR** across the pairwise family — Benjamini & Hochberg
  (1995).

Orientation is `treatment − control`: a negative Δ / δ means the *treatment* mode
scored *lower*.

These paired statistics (computed in-harness via `src/stats/inference.rs`) are the
authoritative result of this evaluation. The pgmcp **experiment ledger** (#65) also
records the raw per-query samples and runs the subsystem's own decision; its
headline test is the *paired Wilcoxon signed-rank* once the criterion is supplied.
A fix in `src/mcp/tools/tool_experiments.rs` makes `experiment_open` accept a
string-encoded `acceptance_criterion` (some MCP argument encoders stringify a
nested object passed to the schema-less param), so a caller can pre-register
`{"type":"wilcoxon_signed_rank","params":{"alpha":0.05,"tail":"two_sided"}}`
directly; it takes effect on the next daemon restart. (The first ledger render
predated the fix and fell back to the kind-default Welch-t, which — being unpaired —
*understated* significance: it could not detect the small-but-consistent
semantic↔hybrid gap that the paired Wilcoxon resolves at `p_adj < 10⁻⁴`. A live
illustration of why pairing matters for per-query IR metrics.)

### 4.5 Corpus

| | value |
|---|---|
| Projects indexed | **97** |
| Files | **68 204** |
| Chunks | **644 386** |
| Embedded chunks | **644 386 (100 %)** |

(The live corpus re-indexes continuously, so these counts — and the absolute
metric decimals below — drift by ≈ ±0.02 between runs; see threat **T7**. The
numbers here are the single canonical campaign of 2026-06-17; conclusions are
invariant across all runs.)

---

## 5. Results

### 5.1 Headline — known-item (N = 50, file granularity)

| mode | MRR | nDCG@10 | Success@1 | recall@10 | crowd@5 |
|---|---:|---:|---:|---:|---:|
| **`semantic`** | **0.301** | **0.405** | **0.140** | **0.740** | 0.037 |
| `hybrid` | 0.225 | 0.345 | 0.060 | 0.740 | 0.037 |
| `text` | 0.017 | 0.023 | 0.000 | 0.040 | 0.000 |

nDCG@10 by mode (▇ = 0.02):

```
semantic  ▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇  0.405
hybrid    ▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇     0.345
text      ▇                     0.023
```

**Pairwise (treatment − control), BH-adjusted (`*` = significant at α = 0.05):**

| metric | control | treatment | Δ | Cliff's δ | magnitude | p_adj |
|---|---|---|---:|---:|---|---:|
| nDCG@10 | semantic | hybrid | −0.060 | −0.114 | negligible | <10⁻⁴ * |
| nDCG@10 | semantic | text | −0.382 | −0.690 | large | <10⁻⁴ * |
| nDCG@10 | hybrid | text | −0.322 | −0.683 | large | <10⁻⁴ * |
| MRR | semantic | hybrid | −0.076 | −0.114 | negligible | <10⁻⁴ * |
| MRR | semantic | text | −0.284 | −0.690 | large | <10⁻⁴ * |
| recall@10 | semantic | text | −0.700 | −0.700 | large | <10⁻⁴ * |
| recall@10 | semantic | hybrid | 0.000 | 0.000 | negligible | n/a (identical) |

**Reading the result.**

1. **Semantic search works** for conceptual code search: a 74 % chance the right
   file is in the top-10, MRR 0.301 (first relevant hit at rank ≈ 3.3 on average).
   `Success@1 = 0.14` is low *by design* — single-gold known-item queries make the
   target compete with its sibling files, and "rank ≤ 10" is the operative bar for
   an agent that reads several results.

2. **Semantic ≫ lexical on conceptual queries** (δ = −0.69, *large*). `text_search`
   finds the gold file in only 4 % of cases: when the query shares no surface
   tokens with the target, FTS has nothing to match. This is the gap dense
   retrieval exists to close, and it does.

3. **Hybrid slightly *hurts* here** (δ = −0.114, *negligible* but significant;
   identical recall@10). RRF rewards a result for appearing in *both* leg lists;
   when the text leg is near-random (conceptual queries), fusing it perturbs the
   top of a strong semantic ranking without adding recall. **Recommendation:** for
   known-conceptual workloads, prefer `semantic_search`, or raise
   `semantic_weight` in `hybrid_search`. Hybrid remains the right default for
   *mixed* workloads that include keyword-precise queries (error strings, symbol
   names) — and §5.3 (M2) shows it *wins* decisively on those.

### 5.2 Leakage-controlled docstring→code (M1 / M3, N = 60)

The rigorous objective leg. A chunk's leading doc-comment is the query and the
chunk is the gold target, but each target is **re-embedded with its doc-comment
removed** (M1) so the stored vector never saw the query, competing against 400
in-full distractor chunks in an isolated database (`pgmcp_m1eval_*`, created and
dropped per run — never the production corpus). **M3** additionally redacts
identifier tokens from query and body.

| stratum | mode | MRR | nDCG@10 | Success@1 | recall@10 |
|---|---|---:|---:|---:|---:|
| **M1** (strip) | **`semantic`** | **0.323** | **0.355** | **0.233** | **0.483** |
| | `hybrid` | 0.321 | 0.353 | 0.233 | 0.483 |
| | `text` | 0.017 | 0.017 | 0.017 | 0.017 |
| **M3** (strip + redact) | **`semantic`** | **0.326** | **0.362** | **0.233** | **0.500** |
| | `hybrid` | 0.324 | 0.360 | 0.233 | 0.500 |
| | `text` | 0.017 | 0.017 | 0.017 | 0.017 |

Pairwise (M1, nDCG@10, BH-adjusted): `semantic` vs `hybrid` Δ = −0.002, δ = −0.003,
**p_adj = 0.10 (not significant)**; `semantic` vs `text` Δ = −0.339, δ = −0.463
(*medium*), `p_adj < 10⁻⁴`.

Three findings:

1. **Genuine, leakage-free semantic retrieval.** With the doc-comment *provably
   absent* from the embedding, semantic search still places the exact target
   chunk in the **top-10 for ≈ 48 %** of queries and at **rank 1 for ≈ 23 %**,
   among 461 chunks. This is the campaign's strongest evidence that dense
   retrieval is doing real semantic matching, not exploiting token overlap
   between the query and the stored chunk.

2. **It is not identifier echo.** Redacting identifiers (M3) *barely* changes
   anything (recall@10 0.483 → 0.500, MRR 0.323 → 0.326) — retrieval survives the
   removal of shared symbol names, so it is matching *meaning*, not *names*.

3. **Hybrid neither helps nor hurts here** (`p_adj = 0.10`). Unlike the known-item
   set (where hybrid was *slightly* worse), on the docstring task `semantic` and
   `hybrid` are statistically indistinguishable — the small known-item penalty is
   a property of that query distribution, not a general regression.

recall@10 (0.48) is lower than the known-item 0.74 because the task is harder and
fully leak-controlled — the query is text the embedding never saw, and the gold is
a single needle. `text`'s near-zero score (0.017) confirms lexical search *cannot*
do leak-free doc→code: once the doc words are stripped from the body, there is
nothing for `ts_rank` to match.

### 5.3 Leak-free full-corpus token-holdout (M2, N = 60)

The third objective leg, run against the **live 644 K-chunk corpus** (no isolated
DB, no re-embedding). For each long chunk (> 512 tokens) the query is a verbatim
snippet drawn from **beyond the 512-token embedding window** — text the chunk's
stored vector never encoded, but which the lexical `content_tsv` indexes in full.
Leak-free by construction against the full distractor set, and a direct probe of
the *truncation frontier*.

| mode | MRR | nDCG@10 | Success@1 | recall@10 | crowd@5 |
|---|---:|---:|---:|---:|---:|
| `semantic` | 0.672 | 0.724 | 0.550 | 0.883 | 0.044 |
| **`hybrid`** | **0.791** | **0.838** | **0.667** | **0.983** | 0.044 |
| `text` | 0.717 | 0.717 | 0.717 | 0.717 | 0.000 |

Pairwise (BH-adjusted): nDCG@10 `semantic` vs `hybrid` Δ = **+0.114**, δ = +0.159
(*small*), **`p_adj = 0.01` ✓** — hybrid significantly better; `semantic` vs `text`
Δ = −0.007 (n.s.). recall@10: **hybrid 0.983 > semantic 0.883 > text 0.717**, every
pairwise gap significant.

Two findings:

1. **Hybrid *wins* on lexical queries — the mirror of known-item.** When the query
   *is* verbatim corpus text, the BM25 leg matches it precisely and RRF fusion
   **significantly beats** pure semantic — the very fusion that slightly *hurt*
   conceptual known-item queries (§5.1). This is the empirical case for the
   `bm25_weight` / `semantic_weight` knobs: **fuse for keyword-precise queries, go
   pure-semantic for conceptual ones.** `text` alone ranks the file #1 whenever it
   finds it (Success@1 = recall@10 = 0.717) but misses ~28 % outright; hybrid's
   semantic leg recovers those (recall@10 0.983).

2. **Truncation is mitigated at the file level.** Although each query is drawn from
   content the *target chunk's* embedding never saw, semantic still reaches the
   right **file** 88 % of the time — a file's overlapping 50-line chunks provide
   redundancy, so the held-out tail is usually embedded in an *adjacent* chunk.
   The 23 % truncation finding (§5.5) is thus a *latent* risk on the dense vector,
   substantially softened by per-file chunk redundancy at retrieval time. (A
   chunk-granularity probe would isolate the residual per-chunk cost; file
   granularity is the operative bar for an agent that opens whole files.)

### 5.4 HNSW honesty + `ef_search` ablation

HNSW recall measured against an exact brute-force scan (`enable_indexscan = off`),
project-scoped, over the 50 known-item queries:

| `ef_search` | recall vs exact (top-10) | mean latency |
|---:|---:|---:|
| 40 | **1.000** | 75.7 ms |
| 100 | **1.000** | 72.5 ms |
| 200 | **1.000** | 73.0 ms |

**The HNSW approximation is lossless at this scale** — every reported hit is a
true nearest neighbor, so an index miss can never be confused with an embedding
miss. `ef_search` has no recall effect here (even 40 is exact) and a flat ~73 ms
latency; the default 100 is a safe, slightly-conservative choice. (At much larger
per-project scales this margin would narrow; the ablation should be re-run if a
single project's chunk count grows by an order of magnitude.)

### 5.5 The 512-token truncation finding

| | value |
|---|---:|
| Total chunks | 644 388 |
| Chunks > 2048 chars (≈ > 512 tokens) | **147 208 (22.8 %)** |
| Mean chunk length | 3 949 chars |
| Max chunk length | 921 600 chars |

BGE-M3 is capped at `max_length = 512` tokens (`src/embed/model.rs`), but doc /
prose / transcript chunks routinely exceed it. **≈ 23 % of the corpus has content
beyond the embedding window** — that tail is silently dropped from the dense
vector (it survives in `content_tsv`, creating a dense/lexical asymmetry), so a
chunk whose answer lives past token 512 is dense-unreachable *at the chunk level*.
**The M2 hold-out (§5.3) measures the retrieval impact directly.** At the **file**
level semantic still reaches the right file ≈ 90 % of the time (recall@10),
because overlapping per-file chunks embed the tail in a neighbour — file-level
retrieval is *softened*. But the **chunk-granularity** M2 pass (`m2_holdout_chunk`,
scoring the exact hold-out chunk by its line span; GPU run 2026-06-19) isolates
the residual cost: semantic recall@10 falls to **0.65** and nDCG@10 to **0.565** —
a **≈ 25-point** per-chunk penalty. The right *file* is found; the truncated
*chunk* ranks below its untruncated neighbours. **Hybrid narrows the gap** (file
0.975 → chunk 0.863) because the dropped tail survives in `content_tsv`, so BM25
still matches it — direct confirmation of the dense/lexical asymmetry. So
truncation is a *latent* lever at the file level but a *real* ≈ 25-point cost at
the chunk level. **Recommendation:** AST/heading-aware sub-chunking to keep
semantic units under the window, or a larger `max_length` for long-form documents.

### 5.6 Pattern-catalog crowding

Across known-item queries whose gold is *not* a pattern file, `src/patterns/*`
catalog cards occupy only **3.7 %** of top-5 slots (semantic and hybrid alike).
The ~810-entry catalog is **not** the dominant distractor that generic ad-hoc
probing implied — well-specified intent queries are not crowded out by it.

### 5.7 Reranker A/B — cross-encoder vs ColBERT (Epic 1)

A flat dense top-k leaves top-rank headroom (known-item Success@1 ≈ 0.14). Two
**local** second-stage rerankers re-score the top-30 `semantic_search`
candidates (carrying chunk content) and reorder the top-20: the
**BGE-reranker-v2-m3 cross-encoder** (`src/reranker/bge_v2_m3.rs`, a single-label
relevance head) and **BGE-M3 ColBERT MaxSim** late-interaction (`embed_colbert` +
`colbert_maxsim`). Both run on the local GPU; scored against the same gold as
§5.1 / §5.3 (GPU/BF16 run, 2026-06-19).

| stratum · mode | MRR | nDCG@10 | Success@1 | recall@10 |
|---|---:|---:|---:|---:|
| **known-item** (N=50) · semantic | 0.303 | 0.405 | 0.140 | 0.740 |
| ·· + cross-encoder | 0.339 | 0.402 | **0.200** | 0.640 |
| ·· + ColBERT | 0.287 | 0.363 | 0.140 | 0.640 |
| **M2 holdout** (N=80) · semantic | 0.698 | 0.746 | 0.575 | 0.900 |
| ·· + cross-encoder | 0.525 | 0.594 | 0.388 | 0.838 |
| ·· + ColBERT | **0.787** | **0.821** | **0.713** | **0.925** |

Significant pairs (BH-adjusted, α = 0.05). On **M2** ColBERT beats semantic
(nDCG δ = +0.126, `p_adj = 0.003`; MRR δ = +0.125, `p_adj = 0.005`) and the
cross-encoder *loses* to semantic (nDCG δ = −0.241 *small*, `p_adj = 0.0008`; MRR
δ = −0.235, `p_adj = 0.001`); ColBERT beats the cross-encoder decisively
(nDCG δ = +0.349 *medium*, `p_adj < 10⁻⁴`). On **known-item** no rerank pair is
significant — the cross-encoder nudges Success@1 0.14 → 0.20 and MRR up, but
trades recall@10 down (0.74 → 0.64) and leaves nDCG flat.

The ColBERT lift is **small** (mean nDCG +0.074, 0.746 → 0.821; Cliff's δ = 0.126,
*negligible*): it is significant under the **paired** Wilcoxon that exploits the
within-query design (`p_adj = 0.003`), but *not* under an unpaired Welch's t
(`p = 0.077`, Cohen's d = 0.23) — the pre-registered test in **experiment #106**
(`docs/scientific-ledger/reranker-a-b-…-2026-06-19.md`), recorded as REJECTED to
avoid p-hacking. So the honest reading is "a real, consistent, *modest* lexical
gain," not a large one.

**Reading.** Reranking is **query-distribution-dependent**, mirroring the hybrid
finding (F6): late-interaction **ColBERT** is a small-but-consistent win on
lexical / verbatim queries (M2) and neutral on conceptual ones (known-item) — its
value is **no downside anywhere**, so it is the safe reranker. The
**cross-encoder** *hurts* verbatim queries (trained on natural-language
query↔passage relevance, it misjudges a text-tail-vs-code pair) while giving only
a non-significant top-rank nudge on conceptual ones, so it must not be enabled
blanket. → **F7**.

### 5.8 Graph-augmented retrieval modes (Epic 3)

pgmcp ships three graph-aware retrieval tools over the code graph
(`code_graph_edges` 1.14 M edges, `file_symbols` 452 K, `code_summary_tree` 759):
**`code_ppr_search`** (Personalized PageRank / HippoRAG → ranked files),
**`code_path_search`** (PathRAG → flow-pruned dependency routes), and
**`code_raptor_search`** (RAPTOR module-cluster summaries). Each is reduced to a
file-granularity ranked list (a route / cluster "hits" if it contains the gold
file) and scored against the §5.1 / §5.3 gold.

| stratum · mode | MRR | nDCG@10 | Success@1 | recall@10 |
|---|---:|---:|---:|---:|
| **known-item** (N=50) · semantic | 0.302 | 0.405 | 0.140 | 0.740 |
| ·· code_ppr | 0.279 | 0.333 | 0.140 | 0.520 |
| ·· code_path | 0.183 | 0.195 | 0.160 | 0.240 |
| ·· code_raptor | 0.004 | 0.000 | 0.000 | 0.000 |
| **M2 holdout** (N=80) · semantic | 0.697 | 0.746 | 0.575 | 0.900 |
| ·· code_ppr | 0.474 | 0.531 | 0.350 | 0.713 |
| ·· code_path | 0.374 | 0.388 | 0.350 | 0.438 |
| ·· code_raptor | 0.000 | 0.000 | 0.000 | 0.000 |

Every graph mode is significantly *below* flat semantic for single-target
retrieval (known-item recall@10: ppr δ = −0.220 `p_adj = 0.003`; path δ = −0.500
*large*; raptor δ = −0.740 *large*; same ordering on M2) — **experiment #107**
(`docs/scientific-ledger/graph-augmented-…-2026-06-19.md`) records the
"graph-beats-flat" hypothesis as REJECTED. This is **expected and correct**: these tools answer *relational* ("how does A reach B") and
*conceptual-module* ("which module owns X") questions, not "find the one file" —
that is `semantic_search`'s job. Among them **`code_ppr` is the strongest** (it
restarts PageRank on the dense hits, so it inherits semantic's recall then
re-ranks by graph proximity) and ties Success@1 with semantic on known-item.
**`code_raptor` scores ≈ 0 at file granularity**: the pgmcp RAPTOR clusters are
very coarse (3 clusters spanning 193 / 105 / 561 files) and the tool returns only
~12 `sample_files` per cluster, so single-file matching is near-impossible — a
real signal that RAPTOR clustering for this corpus is **under-resolved** (its
HDBSCAN / c-TF-IDF parameters want tuning) and that RAPTOR is a *navigation* aid,
not a retrieval ranker. → **F8**.

### 5.9 LLM-as-judge — graded relevance over conceptual queries (Epic 2)

The known-item and M2 strata use objective single-target gold. *Conceptual*
queries ("how does this project handle retries") have no single gold file, so
relevance is **graded by a local LLM judge** over pooled candidates. 40 conceptual
queries (`conceptual_queries()`); for each, the top-10 of semantic / hybrid / text
are **pooled** (TREC-style — union ≈ 9 unique files/query, **361 graded**) and each
`(query, passage)` is graded 0–3 by **DeepSeek-V4-pro** (on the `sparky` DGX Spark)
with a fixed rubric. Grading is **point-wise** (one candidate per call → no list
position bias) and **system-blind** (the judge never sees which mode produced a
candidate); the grades become the graded gold the rank metrics score against.

**Judge reliability.** A **cross-family** second judge (**Qwen3-14B**, also local
on sparky) re-graded a 53-pair sample; quadratic-weighted Cohen's **κ = 0.812** —
"almost perfect" (Landis–Koch). Two independent model *families* agreeing this
strongly means the grades reflect relevance, not one model's idiosyncrasy. The
grade distribution is non-degenerate — {irrelevant 118, marginal 111, relevant 25,
highly-relevant 107} — so the judge discriminates. (qwen3-32B CPU-offloads on the
GB10 ollama and was unusable as the κ partner; see ADR-020.)

| mode | MRR | nDCG@10 | recall@1 | recall@10 |
|---|---:|---:|---:|---:|
| **semantic** | **0.954** | **0.874** | 0.164 | **0.986** |
| hybrid | 0.770 | 0.718 | 0.109 | 0.885 |
| text | 0.338 | 0.110 | 0.040 | 0.086 |

All pairwise differences are significant (BH-adjusted, α = 0.05). **Semantic
decisively beats hybrid** (nDCG δ = −0.598 *large*, `p_adj < 10⁻⁴`; MRR δ = −0.344
*medium*; recall@10 δ = −0.456 *medium*), which in turn dominates text
(nDCG δ = −0.994). (recall@1 is low for *all* modes because conceptual queries
average ≈ 6 judged-relevant files each — 243 relevant / 40 — so one relevant file
at rank 1 is only ≈ 0.16 recall; MRR 0.954 confirms the rank-1 file *is* relevant.)

**Reading.** This is the **strong form of F2**: on *purely* conceptual queries
**fusion hurts**, with a *large* effect (vs the *negligible* known-item gap in
§5.1, whose set deliberately mixes lexical-friendly queries) — BM25's lexical leg
pulls in keyword-matching non-answers that displace semantically-relevant ones,
and an *independent* LLM judge (κ = 0.81) confirms it. Together with F6 (hybrid
*wins* lexical/verbatim M2), the routing rule is now fully evidenced and
consistent: **conceptual → `semantic_search`; keyword/verbatim → `hybrid_search`.**
→ **F9**.

---

## 6. Findings & recommendations

| # | finding | evidence | recommendation |
|---|---|---|---|
| F1 | Semantic search is effective for conceptual queries | recall@10 0.74, MRR 0.30, δ = −0.69 vs text | keep `semantic_search` as the conceptual default |
| F2 | Hybrid slightly hurts on purely-conceptual queries (ties on docstrings) | known-item δ = −0.114, `p_adj < 10⁻⁴`, equal recall@10; M1 δ = −0.003, `p_adj = 0.10` (n.s.) | prefer semantic, or raise `semantic_weight`, for known-conceptual use |
| F6 | Hybrid **wins** on lexical / verbatim queries | M2 nDCG δ = +0.159 *small*, `p_adj = 0.01`; recall@10 hybrid 0.98 > sem 0.88 > text 0.72 | route keyword/exact queries to `hybrid_search` (raise `bm25_weight`); the fuse-vs-pure choice is **query-distribution-dependent** |
| F3 | HNSW is lossless at this scale | recall-vs-exact = 1.000 ∀ ef | leave `ef_search = 100`; re-ablate if a project 10×'s in size |
| F4 | ≈ 23 % of chunks exceed the 512-token window — *file*-level softened, *chunk*-level real | 147 K / 644 K > 2048 chars; M2 **file** recall@10 0.90 but **chunk** recall@10 0.65 (≈ 25 pp per-chunk cost); hybrid chunk 0.86 (tail survives in `content_tsv`) | AST/heading-aware sub-chunking, or larger `max_length` for prose |
| F5 | Pattern-catalog crowding is mild | 3.7 % of top-5 | no action needed |
| F7 | Reranking is query-distribution-dependent | M2: ColBERT nDCG δ = +0.126 (`p_adj` = 0.003); cross-encoder nDCG δ = −0.241 (`p_adj` = 0.0008); known-item all n.s. | enable **ColBERT** rerank as a safe default; gate the **cross-encoder** to conceptual queries only (or leave off) |
| F8 | Graph modes complement, not replace, dense retrieval | known-item + M2 all below semantic; `code_ppr` closest (ties Success@1), `code_raptor` ≈ 0 at file granularity | keep graph tools for relational / module queries; tune the (too-coarse) RAPTOR clustering |
| F9 | On purely-conceptual queries pure semantic beats fusion (*large* effect, LLM-judged) | judge stratum nDCG δ = −0.598 (`p_adj` < 10⁻⁴), recall@10 δ = −0.456; cross-family judge κ = 0.81 | route conceptual → `semantic_search`, keyword/verbatim → `hybrid_search` — this query-distribution routing (with F6) is the load-bearing rule |

---

## 7. Threats to validity

- **T1 — User's own corpus, no public baseline.** All conclusions are *relative*
  and *paired*; a planted-relevance synthetic stratum unit-tests the metric
  pipeline independent of the corpus (`retrieval_metrics.rs` tests).
- **T2 — Author bias (A).** Queries are intent-phrased and identifier-echo-guarded
  (`known_item_queries_avoid_filename_identifier_echo`); strategy B carries no
  author bias.
- **T3 — Docstring leakage (B).** Severed exactly by M1 strip-and-re-embed; M3
  redaction reports the residual identifier-echo gap.
- **T4 — Query-embedding precision parity.** This campaign embeds queries on
  **CPU/F32** while the corpus was embedded on **GPU/BF16**, to avoid contending
  with the daemon's resident GPU workers. The ≈ 10⁻³ cosine perturbation does not
  affect rank order; corroborated by the **HNSW-vs-exact recall = 1.000** check
  (the CPU query vectors still retrieve the exact neighbors). Re-running with
  `--gpu` (when a GPU slot is free) is a parity cross-check.
- **T5 — Single-gold known-item.** `Success@1` is pessimistic when several files
  plausibly answer; recall@10 and MRR are the load-bearing metrics.
- **T6 — Statistical power.** N = 50 (A) powers *moderate+* effects; the
  negligible semantic-vs-hybrid gap is detected only because it is highly
  consistent. M1/M2 (N = 60) enlarge N for the objective legs.
- **T7 — Live-corpus non-stationarity.** The corpus re-indexes continuously
  (editing `src/` during development changes its own chunks), so absolute decimals
  drift ≈ ±0.02 between runs — e.g. semantic known-item nDCG@10 ranged 0.390–0.405
  across four runs. The numbers in this report are one **canonical campaign**
  (2026-06-17); every qualitative conclusion (mode ordering, significance,
  effect-size class) held across all runs. For a frozen baseline, snapshot the
  corpus (or pin a non-self-modifying project) before the run.
- **T8 — M2 granularity (resolved).** The headline token-hold-out is scored at
  file granularity, so a file's overlapping chunks can satisfy a query drawn from
  one chunk's tail — measuring *file-level* truncation robustness. The residual
  *per-chunk* cost is **now measured directly** by the chunk-granularity M2 pass
  (§5.5, `m2_holdout_chunk`): semantic recall@10 **0.65** at chunk granularity vs
  **0.90** at file granularity — a ≈ 25-point per-chunk truncation penalty. (Text
  has no line spans, so only the semantic/hybrid chunk-gran rows are meaningful.)
- **T9 — LLM-judge validity (Epic 2).** Conceptual relevance (§5.9) is graded by
  an LLM (DeepSeek-V4), which could encode that model's biases. Mitigated three
  ways: a fixed 0–3 rubric; **point-wise, system-blind** grading (the judge never
  sees which mode produced a candidate, so it cannot favour one); and a
  **cross-family** agreement check — Qwen3-14B re-graded a 53-pair sample at
  quadratic-weighted Cohen's **κ = 0.81** ("almost perfect"). Only the conceptual
  stratum is LLM-graded; known-item / M1 / M2 use objective gold.

---

## 8. Reproduction

```sh
# Unit tests (metric core + extractor + stats), no DB:
cargo nextest run --release --bin pgmcp retrieval_metrics
cargo nextest run --release -p pgmcp-testing --lib

# Headline + M2 token-holdout + ablations (live DB + CPU embedder; ~3 min; no
# test DB needed — M2 runs on the live corpus, leak-free, no re-embedding):
cargo run --release -p pgmcp-testing --bin eval-retrieval -- --limit 20

# Full campaign — adds the leakage-controlled M1/M3 strata (needs a CREATEDB-
# capable test DB; isolated throwaway databases, never touches production):
PGMCP_TEST_DATABASE_URL="postgres://postgres@localhost:5432/postgres" \
  cargo run --release -p pgmcp-testing --bin eval-retrieval -- \
  --limit 20 --m1 --m1-targets 60 --m1-distractors 400 \
  --out target/eval/retrieval_results_full.json

# Optional GPU parity cross-check (free a daemon GPU slot first):
cargo run --release -p pgmcp-testing --bin eval-retrieval -- --gpu
```

Query sets and gold labels are version-controlled Rust
(`pgmcp-testing/src/eval/query.rs`) with invariant tests, not opaque fixtures —
edits show up as reviewable diffs. Raw per-query samples (aligned by query id) are
written into the results JSON for re-analysis and for the experiment ledger.

---

## 9. Follow-ups — status

The four follow-up epics designed after the original campaign are now
**implemented** (2026-06-19); this section records where each landed.

- **✓ Reranker A/B (Epic 1)** → §5.7, finding **F7**, experiment **#106**
  (`docs/scientific-ledger/reranker-a-b-…-2026-06-19.md`). ColBERT = safe default;
  cross-encoder gated off. (`pgmcp-testing/src/eval/rerank.rs`, `--rerank`.)
- **✓ Graph-augmented modes (Epic 3)** → §5.8, finding **F8**. PPR / PathRAG /
  RAPTOR scored at file granularity; graph *complements*, not replaces, dense
  retrieval. (`pgmcp-testing/src/eval/runner.rs::GraphMode`, `--graph`.)
- **✓ LLM-as-judge pooled relevance (Epic 2)** → §5.9. Local-first: DeepSeek-V4
  primary + a Qwen3 cross-family Cohen's κ, both on the `sparky` DGX Spark.
  (`pgmcp-testing/src/eval/judge.rs`, `--judge`, `PGMCP_JUDGE_*` env.)
- **✓ CI regression gate + drift cron (Epic 3)** → a frozen probe set
  (`src/quality/retrieval_drift.rs`) drives **both** the `#[test]` gate
  (`pgmcp-testing/tests/eval_semantic_quality.rs`, MRR/recall floors, validated
  live at MRR 0.38 / recall@10 0.75) **and** the opt-in `retrieval-eval` cron
  (`src/cron/retrieval_eval.rs`; `[cron] retrieval_eval_interval_secs`).
- **✓ Per-chunk truncation pass (F4 / T8)** → §5.5, the chunk-granularity M2
  variant (`m2_holdout_chunk`).
- **✓ Best-model-per-purpose audit (Epic 4)** → **ADR-020**
  (`docs/decisions/020-best-model-per-purpose.md`).

Genuinely **future** (scoped, not done):
- **AST / heading-aware sub-chunking** to keep semantic units under the 512-token
  window (the F4 mitigation; this harness is ready to measure it).
- **Alternative-dense-embedder bake-off** — a fair comparison requires
  re-embedding the whole corpus with the candidate (a multi-hour GPU job + a new
  `EmbeddingModel` variant); the `eval-retrieval` harness + the leak-controlled M1
  stratum are ready to drive it on `sparky` (see ADR-020).

---

## 10. References

- Järvelin, K. & Kekäläinen, J. (2002). *Cumulated gain-based evaluation of IR
  techniques.* ACM TOIS 20(4). doi:[10.1145/582415.582418](https://doi.org/10.1145/582415.582418)
- Husain, H. et al. (2019). *CodeSearchNet Challenge: Evaluating the State of
  Semantic Code Search.* arXiv:[1909.09436](https://arxiv.org/abs/1909.09436)
- Cormack, G., Clarke, C. & Büttcher, S. (2009). *Reciprocal Rank Fusion
  outperforms Condorcet and individual rank learning methods.* SIGIR.
  doi:[10.1145/1571941.1572114](https://doi.org/10.1145/1571941.1572114)
- Chen, J. et al. (2024). *BGE M3-Embedding: Multi-Lingual, Multi-Functionality,
  Multi-Granularity Text Embeddings.* arXiv:[2402.03216](https://arxiv.org/abs/2402.03216)
- Wilcoxon, F. (1945). *Individual comparisons by ranking methods.* Biometrics
  Bulletin 1(6). doi:[10.2307/3001968](https://doi.org/10.2307/3001968)
- Cliff, N. (1993). *Dominance statistics: Ordinal analyses to answer ordinal
  questions.* Psychological Bulletin 114(3). doi:[10.1037/0033-2909.114.3.494](https://doi.org/10.1037/0033-2909.114.3.494)
- Efron, B. (1987). *Better bootstrap confidence intervals.* JASA 82(397).
  doi:[10.1080/01621459.1987.10478410](https://doi.org/10.1080/01621459.1987.10478410)
- Benjamini, Y. & Hochberg, Y. (1995). *Controlling the false discovery rate.*
  JRSS-B 57(1). doi:[10.1111/j.2517-6161.1995.tb02031.x](https://doi.org/10.1111/j.2517-6161.1995.tb02031.x)
- Malkov, Yu. & Yashunin, D. (2018). *Efficient and robust approximate nearest
  neighbor search using HNSW graphs.* IEEE TPAMI.
  doi:[10.1109/TPAMI.2018.2889473](https://doi.org/10.1109/TPAMI.2018.2889473)
```
