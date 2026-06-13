---
pgmcp_experiment: persistent-vocab-artrie-adaptive-edge-store-get-index-2026-06-13
title: PersistentVocabARTrie adaptive edge-store get_index
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: accepted
p_value: 0.000000
git_ref: 6c8d812823ea3042efadaae6db062c0d039508e7
---

# PersistentVocabARTrie adaptive edge-store get_index

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does the shared sparse adaptive edge store improve PersistentVocabARTrie term-to-index lookup latency?

Vocab builds on the char overlay with u64 values and append-only index semantics. The hypothesis targets lookup performance under the same correctness and non-blocking constraints.

## Hypotheses

**H1.** The shared char/vocab Tiny4/Small16/Sorted64/Hash edge store decreases get_index_ns_per_query for vocabulary workloads without regressing insertion or reverse lookup. — *✅ accepted*

- metric: `get_index_ns_per_query` (ns/query) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 01:54:29Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `get_index_ns_per_query` | welch_t | -5.821860 | 0.000000 | -1.152900 | [-3.6349, -1.7838] | accepted |

**Decision on `get_index_ns_per_query`:**

ACCEPTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-5.8219, p=0.000000, effect=-1.1529, 95% CI=[-3.6349, -1.7838]

Operator note: Fixed-sample vocab benchmark used the seeded Markov vocabulary generator (seed 0x50415254564F4341) with no external corpus. Treatment reduced get_index latency by 3.56% and met the preregistered p < 0.05 threshold.

## What did NOT work

_Nothing rejected (or no decisions yet)._

## Reproducibility

- git ref: `6c8d812823ea3042efadaae6db062c0d039508e7`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 01:54:29Z — **opened**: PersistentVocabARTrie adaptive edge-store get_index
- 2026-06-13 01:54:29Z — **criterion_locked**: The shared char/vocab Tiny4/Small16/Sorted64/Hash edge store decreases get_index_ns_per_query for vocabulary workloads without regressing insertion or reverse lookup.
- 2026-06-13 02:41:40Z — **run**: control_legacy_edge_store (control)
- 2026-06-13 02:41:56Z — **run**: treatment_adaptive_edge_store (treatment)
- 2026-06-13 02:42:41Z — **decided**: accepted on get_index_ns_per_query (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
