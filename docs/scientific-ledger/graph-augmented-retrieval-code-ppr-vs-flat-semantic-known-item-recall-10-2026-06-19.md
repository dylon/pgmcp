---
pgmcp_experiment: graph-augmented-retrieval-code-ppr-vs-flat-semantic-known-item-recall-10
title: Graph-augmented retrieval (code_ppr) vs flat semantic — known-item recall@10
date: 2026-06-19
project: workspace
kind: investigation
status: decided
verdict: rejected
p_value: 0.988672
plan: docs/evaluation/semantic-search-quality.md
---

# Graph-augmented retrieval (code_ppr) vs flat semantic — known-item recall@10

**Kind:** investigation  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does graph-augmented retrieval (code_ppr_search, Personalized PageRank over the code graph) improve recall@10 over flat dense semantic for single-target (known-item) retrieval?

Epic 3. Tests whether the graph-aware retrieval tools replace flat dense retrieval for "find the one file." Expectation (and finding): they do NOT — graph modes are for relational/module queries (§5.8, F8). Measured on the live pgmcp corpus, GPU/BF16, 2026-06-19. Report's paired Wilcoxon: code_ppr recall@10 δ=−0.220, p_adj=0.003 (semantic 0.74 vs code_ppr 0.52).

## Hypotheses

**H1.** code_ppr_search improves known-item recall@10 over flat semantic_search. — *❌ rejected*

- metric: `recall_at_10` (recall) · predicted: increase · planned n/arm: 51
- pre-registered criterion (locked 2026-06-19 19:11:22Z): `{"type": "welch_t", "params": {"tail": "greater", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `recall_at_10` | welch_t | -2.316379 | 0.988672 | -0.463276 | [-0.4085, -0.0315] | rejected |

**Decision on `recall_at_10`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-2.3164, p=0.988672, effect=-0.4633, 95% CI=[-0.4085, -0.0315]

Operator note: code_ppr recall@10 0.52 < semantic 0.74 — the hypothesis (graph improves single-target retrieval) is REJECTED, which IS the finding (F8): graph-augmented modes complement, not replace, dense retrieval. The report's paired Wilcoxon found the gap significant (δ=−0.220, p_adj=0.003). code_ppr is the strongest graph mode (closest to semantic); code_path and code_raptor trail further (§5.8). Use graph tools for relational/module queries, not "find the one file."

## What did NOT work

- `recall_at_10`: rejected (test=welch_t, p=0.988672)

## Reproducibility

- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-19 19:11:22Z — **opened**: Graph-augmented retrieval (code_ppr) vs flat semantic — known-item recall@10
- 2026-06-19 19:11:22Z — **criterion_locked**: code_ppr_search improves known-item recall@10 over flat semantic_search.
- 2026-06-19 19:11:36Z — **run**: control (control)
- 2026-06-19 19:11:37Z — **run**: treatment (treatment)
- 2026-06-19 19:11:49Z — **decided**: rejected on recall_at_10 (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
