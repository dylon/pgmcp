---
pgmcp_experiment: persistent-artrie-u64-unified-adaptive-edge-store-lookup-2026-06-13
title: PersistentARTrieU64 unified adaptive edge-store lookup
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: rejected
p_value: 0.995229
git_ref: 6c8d812823ea3042efadaae6db062c0d039508e7
---

# PersistentARTrieU64 unified adaptive edge-store lookup

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does replacing the standalone u64 edge store with the shared adaptive edge store improve or preserve native u64 lookup latency?

U64 already has native adaptive edge storage. This experiment tests whether unifying the edge-store policy improves the current implementation while preserving non-blocking ArcSwap publication.

## Hypotheses

**H1.** A shared Tiny4/Small16/Sorted64/Hash adaptive edge store decreases lookup_ns_per_query for native u64 sequences compared with the current Inline16/Sorted128/Hash store, or is rejected if not statistically significant. — *❌ rejected*

- metric: `lookup_ns_per_query` (ns/query) · predicted: decrease · planned n/arm: 36
- pre-registered criterion (locked 2026-06-13 01:54:29Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `lookup_ns_per_query` | welch_t | 2.645017 | 0.995229 | 0.523791 | [3.2051, 22.4811] | rejected |

**Decision on `lookup_ns_per_query`:**

REJECTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=2.6450, p=0.995229, effect=0.5238, 95% CI=[3.2051, 22.4811]

Operator note: Fixed-sample u64 benchmark used seeded time-series sequences and u64 values carrying f64::to_bits payloads. After raising the u64 adaptive sorted threshold to 128, treatment remained slower than the legacy native tiering on the preregistered lookup metric; native u64 still beat the encoded u64-as-bytes baseline by 19.26% in the same treatment run, but that is not this hypothesis's primary comparison.

## What did NOT work

- `lookup_ns_per_query`: rejected (test=welch_t, p=0.995229)

## Reproducibility

- git ref: `6c8d812823ea3042efadaae6db062c0d039508e7`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 01:54:29Z — **opened**: PersistentARTrieU64 unified adaptive edge-store lookup
- 2026-06-13 01:54:29Z — **criterion_locked**: A shared Tiny4/Small16/Sorted64/Hash adaptive edge store decreases lookup_ns_per_query for native u64 sequences compared with the current Inline16/Sorted128/Hash store, or is rejected if not statistically significant.
- 2026-06-13 02:42:09Z — **run**: control_legacy_edge_store (control)
- 2026-06-13 02:42:25Z — **run**: treatment_adaptive_edge_store (treatment)
- 2026-06-13 02:42:48Z — **decided**: rejected on lookup_ns_per_query (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
