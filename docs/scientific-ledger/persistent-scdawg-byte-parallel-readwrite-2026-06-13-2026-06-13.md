---
pgmcp_experiment: persistent-scdawg-byte-parallel-readwrite-2026-06-13
title: PersistentScdawg byte parallel read/write native graph
date: 2026-06-13
project: workspace
kind: optimization
status: decided
verdict: accepted
p_value: 0.000000
git_ref: 0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch
---

# PersistentScdawg byte parallel read/write native graph

**Kind:** optimization  |  **Status:** decided  |  **Correction:** benjamini_hochberg

## Method

**Question:** Does the native byte PersistentScdawg graph reduce read latency under parallel readers plus a writer compared with the encoded SCDAWG ARTrie control?

Fixed-sample cargo bench with PERSISTENT_SUFFIX_FIXED_SAMPLES=1, 51 measured samples after 3 warmups. Each replicate uses four reader threads and one writer thread over seeded ASCII strings.

## Hypotheses

**H1.** PersistentScdawg has lower scdawg_byte_parallel_read_write_ns_per_read than the encoded SCDAWG ARTrie control on the seeded ASCII workload. — *✅ accepted*

- metric: `scdawg_byte_parallel_read_write_ns_per_read` (ns/read) · predicted: decrease · planned n/arm: 30
- pre-registered criterion (locked 2026-06-13 05:50:07Z): `{"type": "welch_t", "params": {"tail": "less", "alpha": 0.05, "min_effect": {"kind": "cohens_d", "threshold": 0.5}}}`

## Measurements & Decisions

| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |
|--------|------|-----------|---|--------|--------|--------|
| `scdawg_byte_parallel_read_write_ns_per_read` | welch_t | -56.046005 | 0.000000 | -11.098763 | [-20813.1940, -19373.0654] | accepted |

**Decision on `scdawg_byte_parallel_read_write_ns_per_read`:**

ACCEPTED (criterion: welch_t, correction: BenjaminiHochberg)
  [0] WelchT: statistic=-56.0460, p=0.000000, effect=-11.0988, 95% CI=[-20813.1940, -19373.0654]

Operator note: Measured at git ref 63c0fa4d295753e43f2a0d69b9033eee9aafce5d using PERSISTENT_SUFFIX_FIXED_SAMPLES=1. User indicated CPU utilization was low enough; vmstat interval rows captured around the run showed 87-93% idle and 0% iowait. Full 36 metric/arm vectors are stored in data table libdictenstein.persistent_suffix_native_benchmark_sample_sets under run_id persistent_suffix_native_fixed_2026_06_13_0638z_63c0fa4d.

## What did NOT work

_Nothing rejected (or no decisions yet)._

## Reproducibility

- git ref: `0c1d16a4ad65eee892716487634830e98da1f6cf + working-tree suffix benchmark parallel-control patch`
- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.

## Timeline

- 2026-06-13 05:50:07Z — **opened**: PersistentScdawg byte parallel read/write native graph
- 2026-06-13 05:50:07Z — **criterion_locked**: PersistentScdawg has lower scdawg_byte_parallel_read_write_ns_per_read than the encoded SCDAWG ARTrie control on the seeded ASCII workload.
- 2026-06-13 06:53:14Z — **run**: control_encoded_scdawg_artrie (control)
- 2026-06-13 06:53:31Z — **run**: treatment_native_scdawg_graph (treatment)
- 2026-06-13 06:54:41Z — **decided**: accepted on scdawg_byte_parallel_read_write_ns_per_read (welch_t)

---
_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._
