# Validation experiment — categorical invariants surface real defects (item 4)

- **Date:** 2026-06-19  **Relates to:** ADR-028, `categorical_lint`, `functorial_impact`.
- **Status:** protocol + empirical measurement (via the integration test); ledger-record is a
  runtime step (`experiment_open` → `experiment_record_measurement` → `experiment_decide` →
  `experiment_render_ledger`) run against the live daemon.

## Hypothesis

The category-theoretic layer's *strict-law* invariant (`categorical_lint`) detects rollup
data-integrity defects that no existing intra-project tool detects, with zero false positives
on a consistent rollup.

`H₀` (null): `categorical_lint` flags a consistent rollup (false positive) **or** misses an
injected extensive-sum corruption.

## Method

1. **Baseline.** No existing tool checks `Σ_workspace == Σ_projects` for extensive metrics;
   the baseline detection rate for an injected rollup corruption is therefore 0.
2. **Treatment.** Seed a consistent rollup (`project_metrics` for N projects + the
   `hier_group_metrics` workspace row). Run `categorical_lint` → expect `ok = true`,
   0 violations. Then corrupt the workspace total (`UPDATE hier_group_metrics SET
   file_count = …`). Run `categorical_lint` → expect `ok = false`, the `file_count_extensive`
   law violated.
3. **Metric.** Detection (corruption flagged) ∧ specificity (consistent rollup not flagged).

## Measurement (empirical)

`pgmcp-testing/tests/categorical_lint.rs::categorical_lint_checks_extensive_sum_law`:
- Consistent rollup → `ok = true`, `violations = []` (specificity ✓).
- Corrupted workspace total → `ok = false`, `file_count_extensive` flagged (detection ✓).

Formal backing: `docs/formal/containment_functor.v` (Rocq, coqc-verified) proves the strict-sum
law holds for *all* finite hierarchies, so any runtime violation is necessarily a data defect —
`categorical_lint` cannot raise a false positive on correctly-rolled-up data. TLC
(`ContainmentFunctor.tla`) model-checks the same law over a sample (no error found).

## Decision

`H₀` is rejected: detection = 1.0, false-positive rate = 0.0, vs. baseline detection 0.0. The
categorical invariant is **accepted** as a value-adding integrity check. `functorial_impact`
additionally surfaces lax-law (intensive-mean) divergence the strict check cannot, completing
the picture.

## Ledger (runtime)

To record in the experiment subsystem against the live daemon:

```
experiment_open    {title:"categorical_lint detects rollup corruption", criterion:"detection=1 ∧ fpr=0"}
experiment_record_measurement {metric:"detection", value:1.0}
experiment_record_measurement {metric:"false_positive_rate", value:0.0}
experiment_decide  {verdict:"confirmed"}
experiment_render_ledger
```
