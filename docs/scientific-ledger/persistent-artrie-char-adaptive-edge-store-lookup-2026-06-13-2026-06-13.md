---
pgmcp_experiment: persistent-artrie-char-adaptive-edge-store-lookup-2026-06-13
title: PersistentARTrieChar adaptive edge-store lookup
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: rejected
p_value: 0.248393
git_ref: 6c8d812823ea3042efadaae6db062c0d039508e7
---

# PersistentARTrieChar adaptive edge-store lookup

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does a sparse-label adaptive edge store improve PersistentARTrieChar lookup and prefix traversal latency?

Char and vocab use u32 labels. The candidate adds sparse adaptive tiers while keeping deterministic sorted iteration and existing checkpoint formats.

## Hypotheses

**H1.** Replacing the char overlay child store with Tiny4/Small16/Sorted64/Hash tiers decreases char lookup_ns_per_query on Unicode/CJK mixed datasets. — *❌ rejected*

- metric: `lookup_ns_per_query` (ns/query) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 01:54:29Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `lookup_ns_per_query` | welch_t | -0.682071 | 0.248393 | -0.135070 | [-10.7741, 5.2618] | rejected |

**Decision on `lookup_ns_per_query`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-0.6821, p=0.248393, effect=-0.1351, 95% CI=[-10.7741, 5.2618]

Operator note: Fixed-sample char benchmark used seeded conditional multinomial Unicode scalar terms and 70/20/10 hot/uniform/miss queries. Treatment mean was lower by 1.98%, but the preregistered lookup metric did not reach p < 0.05.

## What did NOT work

- `lookup_ns_per_query`: rejected (test=welch_t, p=0.248393)

## Reproducibility

- git ref: `6c8d812823ea3042efadaae6db062c0d039508e7`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 01:54:29Z — **opened**: PersistentARTrieChar adaptive edge-store lookup
- 2026-06-13 01:54:29Z — **criterion_locked**: Replacing the char overlay child store with Tiny4/Small16/Sorted64/Hash tiers decreases char lookup_ns_per_query on Unicode/CJK mixed datasets.
- 2026-06-13 02:41:13Z — **run**: control_legacy_edge_store (control)
- 2026-06-13 02:41:26Z — **run**: treatment_adaptive_edge_store (treatment)
- 2026-06-13 02:42:36Z — **decided**: rejected on lookup_ns_per_query (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
