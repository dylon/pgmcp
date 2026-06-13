---
pgmcp_experiment: persistent-scdawg-char-parallel-readwrite-2026-06-13
title: PersistentScdawg char parallel read/write native graph
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: accepted
p_value: 0.000000
git_ref: 0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch
---

# PersistentScdawg char parallel read/write native graph

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does the native char PersistentScdawg graph reduce read latency under parallel readers plus a writer compared with the encoded char SCDAWG ARTrie control?

Fixed-sample cargo bench with PERSISTENT_SUFFIX_FIXED_SAMPLES=1, 51 measured samples after 3 warmups. Each replicate uses four reader threads and one writer thread over seeded Unicode strings.

## Hypotheses

**H1.** PersistentScdawgChar has lower scdawg_char_parallel_read_write_ns_per_read than the encoded char SCDAWG ARTrie control on the seeded Unicode workload. — *✅ accepted*

- metric: `scdawg_char_parallel_read_write_ns_per_read` (ns/read) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 05:50:07Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `scdawg_char_parallel_read_write_ns_per_read` | welch_t | -117.106898 | 0.000000 | -23.190622 | [-35568.7078, -34369.2303] | accepted |

**Decision on `scdawg_char_parallel_read_write_ns_per_read`:**

ACCEPTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-117.1069, p=0.000000, effect=-23.1906, 95% CI=[-35568.7078, -34369.2303]

Operator note: Measured at git ref 63c0fa4d295753e43f2a0d69b9033eee9aafce5d using PERSISTENT_SUFFIX_FIXED_SAMPLES=1. User indicated CPU utilization was low enough; vmstat interval rows captured around the run showed 87-93% idle and 0% iowait. Full 36 metric/arm vectors are stored in data table libdictenstein.persistent_suffix_native_benchmark_sample_sets under run_id persistent_suffix_native_fixed_2026_06_13_0638z_63c0fa4d.

## What did NOT work

_Nothing rejected (or no decisions yet)._

## Reproducibility

- git ref: `0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 05:50:07Z — **opened**: PersistentScdawg char parallel read/write native graph
- 2026-06-13 05:50:07Z — **criterion_locked**: PersistentScdawgChar has lower scdawg_char_parallel_read_write_ns_per_read than the encoded char SCDAWG ARTrie control on the seeded Unicode workload.
- 2026-06-13 06:53:46Z — **run**: control_encoded_scdawg_artrie_char (control)
- 2026-06-13 06:53:59Z — **run**: treatment_native_scdawg_graph_char (treatment)
- 2026-06-13 06:54:55Z — **decided**: accepted on scdawg_char_parallel_read_write_ns_per_read (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
