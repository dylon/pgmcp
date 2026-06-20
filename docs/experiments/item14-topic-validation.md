# Validation experiment — new topic-model applications add signal (item 14)

- **Date:** 2026-06-19  **Relates to:** ADR-029, the 11 topic-model applications.
- **Status:** protocol + empirical measurement (via integration tests); ledger-record is a
  runtime step against the live daemon.

## Hypothesis

The new-corpus topic clustering (`work_item_topics` / `commit_topics` / `prompt_topics`, all
over `crate::topic_apps::cluster_corpus`) recovers the latent theme structure of a corpus:
items that are semantically separable cluster into distinct, correctly-labeled themes.

`H₀` (null): the clustering fails to separate a corpus with two known, well-separated theme
groups (assigns them to one cluster, or mislabels).

## Method

1. **Baseline.** No prior tool clusters work-items / commits / prompts into themes (the code
   topic model only covers code chunks); baseline separation is undefined (capability absent).
2. **Treatment.** Construct a labeled corpus with two well-separated embedding groups (group A
   near `e₀`, group B near `e₂`) and run `cluster_corpus(_, k = 2)`.
3. **Metric.** (i) cluster count = 2; (ii) every item assigned to a theme (coverage = 1.0);
   (iii) labels reflect member terms.

## Measurement (empirical)

- `src/topic_apps/mod.rs::tests::clusters_separable_corpus`: two separable groups → exactly
  2 clusters, all 12 items covered, labels drawn from member terms. (deterministic, seed 42.)
- `pgmcp-testing/tests/corpus_topics.rs::corpus_topics_cluster_work_items`: end-to-end through
  `work_item_topics` over two seeded bug embedding clusters → ≥1 theme, all items covered.
- Companion read-application correctness:
  `cross_project_topic_redundancy` (shared-vs-single-project topics),
  `topic_scoped_search`, `topic_drift_warning` (growth flagged), `topic_ownership_forecast`
  (single-owner risk), `topic_experiment_map`, `doc_code_topic_alignment` (JSD in [0,1]) — each
  asserted in its integration test.

## Decision

`H₀` is rejected: the clustering separates the labeled corpus (2 clusters, full coverage,
sensible labels), deterministically. The new topic-model applications are **accepted**. The
clustering reuses the ADR-017 quality gate, so a degenerate model is refused before shipping.

## Ledger (runtime)

```
experiment_open    {title:"work-item topic clustering separates a labeled corpus", criterion:"clusters=2 ∧ coverage=1"}
experiment_record_measurement {metric:"clusters", value:2}
experiment_record_measurement {metric:"coverage", value:1.0}
experiment_decide  {verdict:"confirmed"}
experiment_render_ledger
```
