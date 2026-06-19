---
pgmcp_experiment: pgmcp-semantic-search-retrieval-quality
title: pgmcp semantic-search retrieval quality
date: 2026-06-17
project: workspace
kind: investigation
status: decided
verdict: rejected
p_value: 0.866627
plan: docs/evaluation/semantic-search-quality.md
---

# pgmcp semantic-search retrieval quality

**Kind:** investigation  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Is semantic_search effective for conceptual code retrieval, and does fusing a lexical leg (hybrid_search) change ranking quality versus pure semantic search?

First retrieval-quality evaluation of pgmcp's search. 50 intent-phrased known-item queries over the live 644K-chunk corpus, scored by rank-based metrics (MRR/recall@k/nDCG@10) against human-labeled gold files. Harness: pgmcp-testing/src/bin/eval_retrieval.rs. Full methodology + results: docs/evaluation/semantic-search-quality.md. Paired per-query nDCG@10 with unit_keys → Wilcoxon signed-rank.

## Hypotheses

**H1.** On conceptual (intent-phrased) known-item queries, semantic_search and hybrid_search differ in per-query nDCG@10 (paired, two-sided). — *❌ rejected*

- metric: `ndcg_at_10` (ndcg) · predicted: either · planned n/arm: 51
- pre-registered criterion (locked 2026-06-17 04:34:36Z): `{"type": "welch_t", "params": {"tail": "greater", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `ndcg_at_10` | welch_t | -1.117310 | 0.866627 | -0.223462 | [-0.1670, 0.0467] | rejected |

**Decision on `ndcg_at_10`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-1.1173, p=0.866627, effect=-0.2235, 95% CI=[-0.1670, 0.0467]

Operator note: Headline: semantic_search is the strongest mode (mean nDCG@10 0.390 vs hybrid 0.330 vs text 0.023; n=50 paired known-item queries). This decision tests whether hybrid (treatment) outperforms semantic (control) — it does not; hybrid is slightly lower. The subsystem's kind=investigation default criterion is one-sided Welch-t (d>=0.5); the methodologically-preferred PAIRED Wilcoxon signed-rank (Δ=-0.060, Cliff's δ=-0.120 negligible, p_adj<1e-4) is reported in docs/evaluation/semantic-search-quality.md and reaches the same conclusion: fusing a lexical leg via RRF does not improve conceptual retrieval. semantic vastly outperforms lexical text_search (baseline arm: mean 0.023, Cliff's δ=-0.69 large). A leakage-controlled docstring->code stratum (M1, strip-and-re-embed) independently confirms genuine semantic retrieval: 50% recall@10 / 25% rank-1 with the doc-comment removed from the embedding.

## What did NOT work

- `ndcg_at_10`: rejected (test=welch_t, p=0.866627)

## Reproducibility

- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-17 04:34:36Z — **opened**: pgmcp semantic-search retrieval quality
- 2026-06-17 04:34:36Z — **criterion_locked**: On conceptual (intent-phrased) known-item queries, semantic_search and hybrid_search differ in per-query nDCG@10 (paired, two-sided).
- 2026-06-17 04:35:42Z — **run**: semantic (control)
- 2026-06-17 04:35:52Z — **run**: hybrid (treatment)
- 2026-06-17 04:35:58Z — **run**: text (baseline)
- 2026-06-17 04:36:21Z — **decided**: rejected on ndcg_at_10 (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
