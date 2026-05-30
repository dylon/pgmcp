# pgmcp Database Schema

Reference for the PostgreSQL schema pgmcp installs into its database.
Migrations live in `src/db/migrations.rs`; each `apply_step` call is a
versioned migration.


### Core Tables

```
┌───────────────────┐       ┌────────────────────────┐       ┌──────────────────────┐
│     projects      │       │    indexed_files       │       │    file_chunks       │
├───────────────────┤       ├────────────────────────┤       ├──────────────────────┤
│ id     SERIAL     │──┐    │ id      BIGSERIAL      │──┐    │ id      BIGSERIAL    │
│ workspace_path    │  │    │ project_id INTEGER     │  │    │ file_id BIGINT       │
│ path   TEXT (UQ)  │  │    │ path    TEXT (UQ)      │  │    │ chunk_index INTEGER  │
│ name   TEXT       │  └───→│ relative_path TEXT     │  │    │ content TEXT         │
│ discovered_at TZ  │       │ language TEXT          │  └───→│ start_line INTEGER   │
│ last_scanned  TZ  │       │ size_bytes BIGINT      │       │ end_line INTEGER     │
└───────────────────┘       │ content TEXT           │       │ embedding_v2 (1024)  │
                            │ content_hash BIGINT    │       │ blame_commit TEXT    │
                            │ line_count INTEGER     │       │ blame_author TEXT    │
                            │ truncated BOOLEAN      │       │ blame_date TZ        │
                            │ indexed_at TZ          │       └──────────────────────┘
                            │ modified_at TZ         │        UNIQUE(file_id, chunk_index)
                            └────────────────────────┘

┌───────────────────┐       ┌────────────────────────┐       ┌──────────────────────┐
│   git_commits     │       │  git_commit_chunks     │       │  pgmcp_metadata      │
├───────────────────┤       ├────────────────────────┤       ├──────────────────────┤
│ id    BIGSERIAL   │──┐    │ id      BIGSERIAL      │       │ key   TEXT (PK)      │
│ project_id INT    │  │    │ commit_id BIGINT       │       │ value TEXT           │
│ commit_hash TEXT  │  └───→│ chunk_index INTEGER    │       └──────────────────────┘
│ author TEXT       │       │ content TEXT           │
│ author_date TZ    │       │ embedding_v2 (1024)    │
│ subject TEXT      │       └────────────────────────┘
│ body TEXT         │        UNIQUE(commit_id, chunk_index)
└───────────────────┘
 UNIQUE(project_id, commit_hash)
```

### Analysis Tables

| Table                        | Purpose                                            | Key Columns                                                                  |
|------------------------------|----------------------------------------------------|------------------------------------------------------------------------------|
| `cross_project_similarities` | Materialized chunk-pair similarity from batch scan | `chunk_id_a/b`, `chunk_similarity`, `project_name_a/b`                       |
| `code_topics`                | FCM topic clusters with c-TF-IDF labels            | `label`, `keywords`, `keyword_scores`, `chunk_count`, `file_count`           |
| `chunk_topic_assignments`    | Soft topic membership per chunk (fuzzy clustering) | `chunk_id`, `topic_id`, `membership_score`                                   |
| `git_commit_files`           | Files changed per commit (co-change coupling)      | `commit_id`, `file_path`, `change_type`                                      |
| `code_graph_edges`           | Import, co-change, and semantic edges              | `source_file_id`, `target_file_id`, `edge_type`, `weight`                    |
| `file_metrics`               | Precomputed per-file graph and quality metrics     | `pagerank`, `betweenness`, `instability`, `bug_proneness`, `tech_debt_score` |

### Work-Item Tracker Tables (v4 / v5 / v12 / v16 / v17 / v18)

The work-item / plan / bug tracker; full design in
`docs/decisions/004-work-item-tracker.md` (the zazzy-galaxy addendum covers
v16/v17/v18). Closed dimensions (`kind`, `status`, `severity`, `resolution`,
`link_type`, `finding_source`, `channel`, …) are `TEXT` + `CHECK` built from
closed Rust enums in `src/tracker/` and `src/digest/` (the enum is the single
source of truth).

| Table                                          | Purpose                                              | Key Columns                                                                                          |
|------------------------------------------------|------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `work_items`                                   | Item spine (self-FK tree)                            | `public_id`, `parent_id`/`root_id`, `kind`, `status`, `priority`, `severity` (v12), `weight`, `embedding` (1024), claim cols (v5), `assignee`/`assigned_at`/`assigned_by` (v16) |
| `work_item_bug_details`                        | 1:1 bug sidecar (v12; `kind='bug'`)                  | `item_id` (UQ), `reproduction_steps`, `expected_behavior`, `actual_behavior`, `environment`, `affected_version`, `fixed_in_version`, `root_cause`, `is_regression`, `triaged_at`, `resolution` |
| `work_item_status_history`                     | Append-only transition audit                         | `item_id`, `from_status`, `to_status`, `actor_kind`, `evidence_id`                                   |
| `acceptance_criteria` + `verification_evidence`| Machine-checkable spec + un-fakeable proof ledger    | `criterion_kind`, `coverage_mode`; `verdict`, `source` (`manual` = untrusted, `ci` = trusted)        |
| `scope_negotiations`                           | User-only defer/cancel audit                         | `item_id`, `action`, `actor_kind='user'`, `reason`                                                   |
| `item_relations`                               | Typed DAG edges                                      | `from/to_item_id`, `relation_type` (blocks/depends_on/duplicates/…)                                  |
| `tags` + `work_item_tags`                      | Open-catalog tagging                                 | `slug`; `(item_id, tag_id)`                                                                           |
| `plan_definitions` + `definition_rules`        | Reusable plan-shape validation                       | `slug`, `body_toml`; `rule_kind`, `applies_to_kind`, `field_name`                                    |
| `work_item_claims` + `agent_presence`          | A2A claim ledger + presence decay (v5)               | `agent_id`, `action`, `lease_expires_at`                                                             |
| `work_item_git_links`                          | Item ↔ commit/PR/branch join (v17)                   | `item_id`, `link_type` (CHECK), `ref_value`, `commit_id` (FK→`git_commits`, SET NULL), `detected_by` (`manual`/`auto_scan`); **UNIQUE(`item_id`,`link_type`,`ref_value`)** |
| `work_item_finding_provenance`                 | Idempotency ledger for cron auto-promotion (v17)     | **`provenance_key` (UNIQUE)**, `item_id`, `finding_source` (CHECK), `first_seen_at`/`last_seen_at`   |
| `digest_emissions`                             | Proactive-digest rate-limit ledger (v18)             | `session_id`, `channel` (CHECK), `project_id`, `content_sha256`, `item_count`, `ts` (fingerprint-only — no digest body) |

The v16 columns add a **durable ownership** axis to the spine
(`assignee`/`assigned_at`/`assigned_by`, all nullable free-text, set via
`work_item_assign`, never auto-cleared), deliberately distinct from the v5
**ephemeral lease** (`claimed_by`, which auto-expires and clears on
release/handoff/expiry). The partial index `idx_work_items_assignee
(assignee, priority DESC) WHERE assignee IS NOT NULL` serves the `my-work` queue.

Closed vocabularies (Rust enum → CHECK): `kind` (15, incl. `bug`), `status` (12:
…`triage` → `confirmed` → … alongside the verify lifecycle), `severity`
(`critical | high | medium | low`), `resolution` (`fixed | wont_fix | duplicate |
cannot_reproduce | by_design`); `work_item_git_links.link_type`
(`commit | pr | branch`, `GitLinkType`), `work_item_finding_provenance.
finding_source` (`bug_prediction | documented_tech_debt`, `FindingSource`),
`digest_emissions.channel` (`session_start | prompt | webhook`, `DigestChannel`).
The `SmartView` (`my-work | needs-triage | overdue | blocked | next-actionable`)
and `BulkOp` (`set_status | tag | untag | reprioritize | assign`) enums in
`src/tracker/views.rs` are **request-shaping** params, not stored columns, so
they back no DB CHECK.

`quality_report_history` (v9) is now populated by the **`quality-history`** cron
(`src/cron/quality_history.rs`) — it snapshots each project's pillar GPAs every
tick so the trend/forecast tools (`quality_trend`/`quality_forecast`) and the
proactive digest read a *trajectory* rather than a single point.

### Indices

| Index                             | Type                             | Purpose                                   |
|-----------------------------------|----------------------------------|-------------------------------------------|
| `idx_chunks_embedding`            | HNSW (m=24, ef_construction=200) | Cosine similarity for semantic search     |
| `idx_git_commit_chunks_embedding` | HNSW (m=24, ef_construction=200) | Cosine similarity for git commit searches |
| `idx_files_fts`                   | GIN (tsvector)                   | Full-text search on file content          |
| `idx_files_path_trgm`             | GIN (pg_trgm)                    | Trigram similarity for path matching      |
| `idx_files_content_hash`          | B-tree                           | Fast skip-if-unchanged lookups            |
| `idx_files_project`               | B-tree                           | Filter files by project                   |
| `idx_files_language`              | B-tree                           | Filter files by language                  |
| `idx_git_commits_project`         | B-tree                           | Filter git commits by project             |
| `idx_cge_source`                  | B-tree                           | Graph edge source lookups                 |
| `idx_cge_target`                  | B-tree                           | Graph edge target lookups                 |
| `idx_cge_project_type`            | B-tree                           | Graph edges by project and type           |
| `idx_fm_project`                  | B-tree                           | File metrics by project                   |

---

### Scientific-Experiment Tables

The structured source of truth for recorded experiments (rendered to the
`docs/scientific-ledger/*.md` ledgers). Created by
`ensure_experiment_tables` / `ensure_experiment_hnsw_index`. Full design:
`docs/experiments/README.md`.

| Table | Purpose |
|-------|---------|
| `experiments` | Root: question/context, `kind`, status, hardware, links, embedding; bi-temporal (`valid_from`/`valid_to`/`superseded_by`) |
| `experiment_code_anchor` | file / chunk / topic anchors (mirrors `memory_code_anchor`) |
| `experiment_hypotheses` | statement + `primary_metric` + **pre-registered** `acceptance_criterion` JSONB + `criterion_locked_at` + `verdict` + embedding |
| `experiment_runs` | one arm execution (UUID PK); `command_spec` / `run_plan` / `host_meta` JSONB |
| `experiment_samples` | raw per-replicate samples (`is_warmup`, `unit_key` for paired tests) |
| `experiment_results` | the decision: `test_type`, `p_value`, `effect_size`, CI, `verdict`, `criterion_snapshot`, full `test_result` JSONB, embedding |
| `experiment_artifacts` | ad-hoc profiling/benchmark/debug captures (perf/hyperfine/criterion/massif/flamegraph/log) |

Enums: `experiment_kind`, `experiment_status`, `hypothesis_verdict`,
`experiment_arm_kind`, `effect_direction`. All four embedding columns are
`vector(1024)` (BGE-M3) with HNSW indexes (`m=24, ef_construction=200`),
populated synchronously on write and backfilled by the embedding-migration cron.

---

