---
pgmcp_experiment: persistent-artrie-byte-adaptive-edge-store-lookup-2026-06-13
title: PersistentARTrie byte adaptive edge-store lookup
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: rejected
p_value: 0.291131
git_ref: 6c8d812823ea3042efadaae6db062c0d039508e7
---

# PersistentARTrie byte adaptive edge-store lookup

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does a byte-specialized adaptive edge store improve PersistentARTrie byte lookup latency compared with the baseline sorted heap child store?

Baseline overlay ChildStore uses Inline <=4 and sorted heap vectors for all higher fanout. The candidate adds byte-specialized ART tiers while preserving lock-free COW publication and swizzled children.

## Hypotheses

**H1.** Replacing the byte overlay child store with Tiny4/Small16/ByteIndexed48/ByteDense256 tiers decreases lookup_ns_per_query for high-fanout and mixed byte datasets. — *❌ rejected*

- metric: `lookup_ns_per_query` (ns/query) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 01:54:29Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `lookup_ns_per_query` | welch_t | -0.551887 | 0.291131 | -0.109290 | [-10.7086, 6.0477] | rejected |

**Decision on `lookup_ns_per_query`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-0.5519, p=0.291131, effect=-0.1093, 95% CI=[-10.7086, 6.0477]

Operator note: Fixed-sample byte benchmark used seeded conditional multinomial byte terms and 70/20/10 hot/uniform/miss queries. Treatment mean was lower by 1.65%, but not statistically significant at p < 0.05; secondary parallel-read/write samples showed a significant improvement but were not the preregistered primary metric.

## What did NOT work

- `lookup_ns_per_query`: rejected (test=welch_t, p=0.291131)

## Reproducibility

- git ref: `6c8d812823ea3042efadaae6db062c0d039508e7`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 01:54:29Z — **opened**: PersistentARTrie byte adaptive edge-store lookup
- 2026-06-13 01:54:29Z — **criterion_locked**: Replacing the byte overlay child store with Tiny4/Small16/ByteIndexed48/ByteDense256 tiers decreases lookup_ns_per_query for high-fanout and mixed byte datasets.
- 2026-06-13 02:40:47Z — **run**: control_legacy_edge_store (control)
- 2026-06-13 02:41:01Z — **run**: treatment_adaptive_edge_store (treatment)
- 2026-06-13 02:42:32Z — **decided**: rejected on lookup_ns_per_query (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
