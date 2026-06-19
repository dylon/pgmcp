---
pgmcp_experiment: llm-judge-conceptual-relevance-does-hybrid-fusion-beat-pure-semantic
title: LLM-judge conceptual relevance — does hybrid fusion beat pure semantic?
date: 2026-06-19
project: workspace
kind: investigation
status: decided
verdict: rejected
p_value: 0.999996
plan: docs/evaluation/semantic-search-quality.md
---

# LLM-judge conceptual relevance — does hybrid fusion beat pure semantic?

**Kind:** investigation  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** On conceptual queries with LLM-graded relevance, does hybrid (BM25+vector RRF) fusion improve nDCG@10 over pure semantic?

Epic 2. 40 conceptual queries, top-10 of semantic/hybrid/text pooled (361 candidates), graded 0-3 by DeepSeek-V4-pro on sparky (point-wise, system-blind). Judge reliability: cross-family quadratic Cohen's κ=0.81 vs Qwen3-14B (n=53). Tests whether fusion (the BM25 leg) helps PURELY conceptual queries. See §5.9, finding F9. Expectation: it hurts (the strong form of F2).

## Hypotheses

**H1.** hybrid_search improves conceptual-query nDCG@10 over semantic_search (LLM-judged graded relevance). — *❌ rejected*

- metric: `ndcg_at_10` (ndcg) · predicted: increase · planned n/arm: 51
- pre-registered criterion (locked 2026-06-19 19:38:10Z): `{"type": "welch_t", "params": {"tail": "greater", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `ndcg_at_10` | welch_t | -4.805072 | 0.999996 | -1.074447 | [-0.2205, -0.0913] | rejected |

**Decision on `ndcg_at_10`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-4.8051, p=0.999996, effect=-1.0744, 95% CI=[-0.2205, -0.0913]

Operator note: Hybrid (0.718) < semantic (0.874) on LLM-judged conceptual nDCG@10 — the "fusion helps conceptual" hypothesis is REJECTED; pure semantic wins decisively (report's paired Wilcoxon: δ=−0.598 LARGE, p_adj<1e-4). This is the strong form of F2/F9: BM25's lexical leg displaces semantically-relevant answers on purely-conceptual queries. Judge reliability: cross-family quadratic Cohen's κ=0.81 (DeepSeek-V4 vs Qwen3-14B, n=53), almost-perfect — the grades are trustworthy. Routing rule (with F6): conceptual→semantic_search, keyword/verbatim→hybrid_search.

## What did NOT work

- `ndcg_at_10`: rejected (test=welch_t, p=0.999996)

## Reproducibility

- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-19 19:38:10Z — **opened**: LLM-judge conceptual relevance — does hybrid fusion beat pure semantic?
- 2026-06-19 19:38:10Z — **criterion_locked**: hybrid_search improves conceptual-query nDCG@10 over semantic_search (LLM-judged graded relevance).
- 2026-06-19 19:38:30Z — **run**: control (control)
- 2026-06-19 19:38:33Z — **run**: treatment (treatment)
- 2026-06-19 19:38:47Z — **decided**: rejected on ndcg_at_10 (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
