# pgmcp Git History Indexing

Opt-in per-project indexing of git commit messages + bodies + diff stats.
See [the README](../README.md) for the higher-level integration story.


pgmcp can index git commit history (messages and diffs) for projects that opt in,
making your project's development history searchable via vector embeddings.

### Enabling

Add a `.pgmcp.toml` file to your project root (or use `pgmcp init-project`):

```toml
[git]
index_history = true
```

### How It Works

- **Incremental indexing** -- pgmcp tracks the last-indexed commit SHA per project
  in the `pgmcp_metadata` table. Only new commits since the last run are processed.
- **Commit extraction** -- for each new commit, the subject, body, author, date,
  and full diff are extracted via `git log`.
- **Chunking and embedding** -- commit content (message + diff) is chunked and
  embedded into the same vector space as file chunks, stored in `git_commits` and
  `git_commit_chunks` tables.
- **Blame metadata** -- file chunks are annotated with `blame_commit`,
  `blame_author`, and `blame_date` columns, linking code to the commit that last
  touched it.
- **Co-change tracking** -- the `git_commit_files` table records which files
  changed in each commit, enabling co-change coupling analysis via
  `find_coupled_files`.
- **Cron job** -- the `git-history-index` job runs every hour by default
  (configurable via `git_history_index_interval_secs` in the `[cron]` section).

---

