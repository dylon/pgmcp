---
pgmcp_experiment: reranker-a-b-colbert-maxsim-vs-flat-semantic-on-lexical-m2-retrieval
title: Reranker A/B — ColBERT MaxSim vs flat semantic on lexical (M2) retrieval
date: 2026-06-19
project: workspace
kind: investigation
status: decided
verdict: rejected
p_value: 0.076526
plan: docs/evaluation/semantic-search-quality.md
---

# Reranker A/B — ColBERT MaxSim vs flat semantic on lexical (M2) retrieval

**Kind:** investigation  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does a second-stage local reranker improve nDCG@10 over flat dense semantic retrieval, and is the effect query-distribution-dependent (lexical M2 vs conceptual known-item)?

Epic 1 of the semantic-search evaluation follow-ups. Flat dense top-k leaves top-rank headroom (known-item Success@1 ~0.14). Two local rerankers re-score the top-30 semantic candidates: BGE-reranker-v2-m3 cross-encoder and BGE-M3 ColBERT MaxSim. Measured on the live pgmcp corpus, GPU/BF16, 2026-06-19. Paired Wilcoxon in the report. See docs/evaluation/semantic-search-quality.md §5.7.

## Hypotheses

**H1.** ColBERT MaxSim late-interaction reranking of the top-30 semantic_search candidates raises nDCG@10 vs flat semantic on lexical/verbatim (M2 token-holdout) queries (paired, treatment > control). — *❌ rejected*

- metric: `ndcg_at_10` (ndcg) · predicted: increase · planned n/arm: 51
- pre-registered criterion (locked 2026-06-19 18:30:58Z): `{"type": "welch_t", "params": {"tail": "greater", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `ndcg_at_10` | welch_t | 1.435783 | 0.076526 | 0.227017 | [-0.0279, 0.1762] | rejected |

**Decision on `ndcg_at_10`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=1.4358, p=0.076526, effect=0.2270, 95% CI=[-0.0279, 0.1762]

Operator note: ColBERT MaxSim rerank vs flat semantic on the M2 (lexical/verbatim) holdout, N=80 paired. Mirrors the report's paired Wilcoxon (nDCG δ=+0.126, p_adj=0.003, §5.7). The cross-encoder, by contrast, significantly HURT M2 (nDCG δ=−0.241) — reranking is query-distribution-dependent (F7).

## What did NOT work

- `ndcg_at_10`: rejected (test=welch_t, p=0.076526)

## Reproducibility

- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-19 18:30:58Z — **opened**: Reranker A/B — ColBERT MaxSim vs flat semantic on lexical (M2) retrieval
- 2026-06-19 18:30:58Z — **criterion_locked**: ColBERT MaxSim late-interaction reranking of the top-30 semantic_search candidates raises nDCG@10 vs flat semantic on lexical/verbatim (M2 token-holdout) queries (paired, treatment > control).
- 2026-06-19 18:31:36Z — **run**: control (control)
- 2026-06-19 18:31:44Z — **run**: treatment (treatment)
- 2026-06-19 18:31:54Z — **decided**: rejected on ndcg_at_10 (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
