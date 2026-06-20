# ADR-029: New topic-model applications

- **Status:** Accepted — **all 11 applications delivered + tested** (the "roadmap" list below
  is now the as-built inventory).
- **Date:** 2026-06-19
- **Relates to:** item 14, ADR-017 (topic-clustering redesign). Tools:
  `cross_project_topic_redundancy`, `work_item_topics`, `commit_topics`, `prompt_topics`,
  `topic_scoped_search`, `topic_quality_forecast`, `doc_code_topic_alignment`,
  `topic_experiment_map`, `topic_drift_warning`, `topic_ownership_forecast`, + vector-seeded
  `topic` node in `memory_unified_nodes` (#5). Engine: `src/topic_apps/` (`cluster_corpus`).

## Context

pgmcp's topic model (`code_topics` + `chunk_topic_assignments`, the graph-engine clustering of
ADR-017) was applied only to *code chunks* and surfaced via discovery/hierarchy/owner tools.
The user asked for **more topic-model applications for SE intelligence** — extending the model
to new corpora and new analytical lenses.

## Decision

### Landed: `cross_project_topic_redundancy`

A new application of the existing **global** topic model: surface topics whose chunks span
multiple projects (`code_topics.scope = 'global'`, `project_count ≥ N`). Such topics are
**shared concerns / fork-redundancy** — strong cross-project consolidation candidates. It is a
pure read over data the topic-clustering cron already produces (no new clustering, no
migration), ranked by spread then size. This is the topic model applied to *cross-project*
intelligence — complementary to the per-topic `topic_project_map` (which goes topic→projects);
this goes "which topics are most duplicated across the workspace."

## Delivered applications (as-built)

Each reuses the ADR-017 quality gate (`src/quality/topic_metrics.rs`) so a degenerate model
can't ship, and the FCM loop (`crate::fcm::run_seeded`, via `src/topic_apps::cluster_corpus`)
for new-corpus clustering. **All 11 shipped:**

1. **Commit-message topic model** — `commit_topics` over `git_commit_chunks.embedding_v2`. ✅
2. **Bug / work-item topic model** — `work_item_topics` over `work_items.embedding`
   (optional `kind` filter). ✅
3. **Topic-drift early-warning** — `topic_drift_warning` over the `topics_size_history`
   snapshots (emerging/declining themes). ✅
4. **Topic-scoped search** — `topic_scoped_search` (semantic search within a topic's chunks). ✅
5. **Vector-seeded `topic` graph node** — the `topic` arm of `memory_unified_nodes` now seeds
   its embedding from the representative chunk; `in_topic` (chunk→topic) + memory→topic edges
   already present → PPR / PathRAG over topics. ✅
6. **Prompt/conversation topic model** — `prompt_topics` over `session_prompts.embedding_v2`. ✅
7. **Topic ⊗ experiment map** — `topic_experiment_map` (experiment_code_anchor.topic_id). ✅
8. **Topic ownership forecasting** — `topic_ownership_forecast` (git-blame concentration +
   bus_factor + Herfindahl + recency trend → single-owner risk). ✅
9. **Doc/code topic alignment** — `doc_code_topic_alignment` (Jensen-Shannon divergence). ✅
10. **Cross-project fork-redundancy** — `cross_project_topic_redundancy`. ✅
11. **Topic-quality forecast** — `topic_quality_forecast` (OLS trend + ETA over the
    architecture-quality history the topic model feeds). ✅

Validation experiment: `docs/experiments/item14-topic-validation.md` (protocol + empirical
measurement via the integration tests; ledger recorded at runtime via the `experiment_*` tools).

## Consequences

- The topic model now reaches a cross-project SE-intelligence question it couldn't answer
  before, with zero new clustering cost.
- All 11 are independent slices (new corpora via `cluster_corpus`, or reads over existing
  topic/assignment tables), each behind the existing quality gate.
- Tested: real-DB tests for every tool (`cross_project_topic_redundancy`, `corpus_topics`,
  `topic_apps2`, `topic_apps3`, `topic_experiment_map`) + the `cluster_corpus` unit tests +
  the Layer-D coverage gate (every dispatched tool has an integration test).
