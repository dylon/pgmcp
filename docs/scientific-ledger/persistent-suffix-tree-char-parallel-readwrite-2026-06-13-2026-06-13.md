---
pgmcp_experiment: persistent-suffix-tree-char-parallel-readwrite-2026-06-13
title: PersistentSuffixTree char parallel read/write native graph
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: accepted
p_value: 0.000000
git_ref: 0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch
---

# PersistentSuffixTree char parallel read/write native graph

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does the native char PersistentSuffixTree graph reduce read latency under parallel readers plus a writer compared with the encoded char suffix-tree ARTrie control?

Fixed-sample cargo bench with PERSISTENT_SUFFIX_FIXED_SAMPLES=1, 51 measured samples after 3 warmups. Each replicate uses four reader threads and one writer thread over seeded Unicode strings.

## Hypotheses

**H1.** PersistentSuffixTreeChar has lower suffix_tree_char_parallel_read_write_ns_per_read than the encoded char suffix-tree ARTrie control on the seeded Unicode workload. — *✅ accepted*

- metric: `suffix_tree_char_parallel_read_write_ns_per_read` (ns/read) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 05:49:50Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `suffix_tree_char_parallel_read_write_ns_per_read` | welch_t | -126.423732 | 0.000000 | -25.035630 | [-35230.2212, -34128.3486] | accepted |

**Decision on `suffix_tree_char_parallel_read_write_ns_per_read`:**

ACCEPTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-126.4237, p=0.000000, effect=-25.0356, 95% CI=[-35230.2212, -34128.3486]

Operator note: Measured at git ref 63c0fa4d295753e43f2a0d69b9033eee9aafce5d using PERSISTENT_SUFFIX_FIXED_SAMPLES=1. User indicated CPU utilization was low enough; vmstat interval rows captured around the run showed 87-93% idle and 0% iowait. Full 36 metric/arm vectors are stored in data table libdictenstein.persistent_suffix_native_benchmark_sample_sets under run_id persistent_suffix_native_fixed_2026_06_13_0638z_63c0fa4d.

## What did NOT work

_Nothing rejected (or no decisions yet)._

## Reproducibility

- git ref: `0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 05:49:50Z — **opened**: PersistentSuffixTree char parallel read/write native graph
- 2026-06-13 05:49:50Z — **criterion_locked**: PersistentSuffixTreeChar has lower suffix_tree_char_parallel_read_write_ns_per_read than the encoded char suffix-tree ARTrie control on the seeded Unicode workload.
- 2026-06-13 06:52:45Z — **run**: control_encoded_suffix_tree_artrie_char (control)
- 2026-06-13 06:53:01Z — **run**: treatment_native_suffix_tree_graph_char (treatment)
- 2026-06-13 06:54:34Z — **decided**: accepted on suffix_tree_char_parallel_read_write_ns_per_read (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
