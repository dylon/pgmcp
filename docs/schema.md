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
└───────────────────┘       │ content TEXT           │       │ embedding vec(384)   │
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
│ author_date TZ    │       │ embedding vec(384)     │
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

