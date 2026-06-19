# ADR-029: New topic-model applications

- **Status:** Accepted (cross-project redundancy landed; the further applications are the
  roadmap below)
- **Date:** 2026-06-19
- **Relates to:** item 14, ADR-017 (topic-clustering redesign). Tool:
  `cross_project_topic_redundancy`.

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

## Roadmap (further applications)

Each reuses the ADR-017 quality gate (`src/quality/topic_metrics.rs`) so a degenerate model
can't ship, and the FCM loop (`crate::fcm::run_seeded`) for new-corpus clustering:

1. **Commit-message topic model** — cluster `git_commit_chunks.embedding_v2` → development
   themes over time.
2. **Bug / work-item topic model** — cluster `work_items.embedding` → recurring defect themes
   + severity mix + recency slope (the tracker's "what keeps breaking").
3. **Topic-drift early-warning** joined to `bug_prediction`.
4. **Topic-scoped `semantic_search` / `hybrid_search`** filter (`topic_id` / `topic_label`).
5. **Vector-seeded `topic` graph node** + `in_topic` / `topic_in_project` edges → PPR / PathRAG
   over topics.
6. **Prompt/conversation topic model** over `session_prompts`.
7. **Topic ⊗ experiment map** (`experiments.embedding`).
8. **Topic ownership forecasting** (extend `topic_owners` with a blame-date trajectory →
   ETA-to-single-owner via `forecast::weeks_to_threshold`).
9. **Doc/code topic alignment** (JS-divergence of doc-topic vs code-topic distributions).
10. **Cross-project fork-redundancy** — *landed above*.
11. **Topic-quality forecast** wired into `quality_forecast` / digest TREND.

A validation experiment (e.g. bug-topic early-warning vs baseline) decides each application's
value via `experiment_open` → `record_measurement` → `decide` → `render_ledger`.

## Consequences

- The topic model now reaches a cross-project SE-intelligence question it couldn't answer
  before, with zero new clustering cost.
- The roadmap items are independent slices (mostly new `code_topics` scopes / sibling
  collectors over new corpora), each shippable behind the existing quality gate.
- Tested: `cross_project_topic_redundancy` real-DB test (shared vs single-project topics) +
  the Layer-D coverage gate.
