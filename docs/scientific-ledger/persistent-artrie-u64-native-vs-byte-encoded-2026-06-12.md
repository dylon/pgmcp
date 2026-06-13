---
pgmcp_experiment: persistent-artrie-u64-native-vs-byte-encoded
title: PersistentARTrieU64 native edge representation vs byte-encoded facade
date: 2026-06-12
project: workspace
kind: optimization
status: decided
verdict: accepted
p_value: 0.000000
git_ref: 846e66614ed5ccc9d0ab500f12d3a0f447508c4d
---

# PersistentARTrieU64 native edge representation vs byte-encoded facade

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does storing u64 units as native u64 edges make PersistentARTrieU64 lookup faster than the prior eight-byte encoded facade while preserving persistence behavior?

The old PersistentARTrieU64 facade encoded every u64 key unit as eight little-endian byte transitions through PersistentARTrie. The native implementation stores u64 paths directly in a DynamicDawgU64-backed persistent surface with native snapshot/WAL persistence. Control arm is control_encoded_u64_as_bytes; treatment arm is treatment_native_u64.

## Hypotheses

**H1.** The native PersistentARTrieU64 representation has lower lookup_ns_per_query than EncodedPersistentARTrieU64 on deterministic fixed-length u64 sequence workloads. — *✅ accepted*

- metric: `lookup_ns_per_query` (ns/query) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-12 19:26:05Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `lookup_ns_per_query` | welch_t | -202.412891 | 0.000000 | -36.955369 | [-1161.0996, -1138.4745] | accepted |

**Decision on `lookup_ns_per_query`:**

ACCEPTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-202.4129, p=0.000000, effect=-36.9554, 95% CI=[-1161.0996, -1138.4745]

Operator note: Samples were collected from benches/persistent_artrie_u64_native_benchmarks.rs fixed-sample mode after replacing the first linear-edge native attempt with the adaptive u64 edge store. Control is the byte-encoded facade; treatment is the adaptive native u64 representation.

## What did NOT work

_Nothing rejected (or no decisions yet)._

## Reproducibility

- git ref: `846e66614ed5ccc9d0ab500f12d3a0f447508c4d`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-12 19:26:05Z — **opened**: PersistentARTrieU64 native edge representation vs byte-encoded facade
- 2026-06-12 19:26:05Z — **criterion_locked**: The native PersistentARTrieU64 representation has lower lookup_ns_per_query than EncodedPersistentARTrieU64 on deterministic fixed-length u64 sequence workloads.
- 2026-06-12 19:32:43Z — **run**: control (control)
- 2026-06-12 19:32:56Z — **run**: treatment (treatment)
- 2026-06-12 19:35:07Z — **decided**: accepted on lookup_ns_per_query (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
