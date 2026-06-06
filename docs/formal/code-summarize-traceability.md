# Code Summarize Formal Verification Traceability

Status: focused high-use summarization slice for `code_summarize`.

## Scope

`code_summarize` is a read-only structural roll-up over project files, key
metric rows, topics, languages, and effect summaries. It is high-use and
operator-facing, so its main correctness boundary is precise scoping rather
than mutation safety.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `code_summarize` | Trim project/scope/path/detail inputs; resolve exactly one project id; reject unknown `scope`/`detail`; require `path` for directory/file scopes; apply the same literal path filter to directory rollups, key files, and language counts; join file metrics only when metric and file project ids agree; select topics by `project_names` membership; enrich effects using the same resolved project id; omit topics for `detail=brief`. | `tla/CodeSummarizeScope.tla`; `pgmcp-testing/tests/oracle_code_summarize.rs`; `pgmcp-testing/tests/tool_scorecard_integration.rs`. |

## Issues Found And Corrected

The tool resolved projects with `SELECT id FROM projects WHERE name = $1`,
which silently picked an arbitrary row when display names were duplicated.
Correction: it now uses the shared unique project resolver and fails closed on
blank, missing, or duplicate names.

`scope` and `detail` were accepted as raw strings. Correction: both are trimmed,
defaulted, and validated against closed sets.

Directory/file `path` filtering was hand-built into one SQL string and applied
only to the directory rollup. Top files and language totals still reported the
whole project. Correction: path filters are bound SQL parameters with LIKE
wildcard escaping, and the same filter applies to directory summaries, key
files, and language breakdown.

Top-file PageRank joined `file_metrics` by `file_id` alone. Correction: it now
requires `file_metrics.project_id = indexed_files.project_id`.

Topic lookup used `scope LIKE '%project'`. Correction: it now requires the
normalized project name to appear in `code_topics.project_names`.

Effect enrichment performed a second project-name lookup. Correction: it reuses
the already resolved project id.

## Concurrency Boundary

This slice is read-only and introduces no locks or spawned work. Concurrent
indexing can change what a later summary sees, but each call uses one resolved
project id and one normalized path predicate for all file-derived output
channels. That prevents cross-project leakage and internal summary mismatch
within the response.

## Formal Model

`tla/CodeSummarizeScope.tla` models unique, duplicate, and missing project
names; project/directory/file scopes; brief/standard/detailed detail modes;
required path handling; literal directory prefix matching for wildcard-looking
paths; stale cross-project metric rows; project-scoped topics; and effect
enrichment.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `UniqueProjectRequired` | Blank, missing, or duplicate project names return no rows. |
| `ScopeAndDetailValidated` | Successful responses use only accepted scope/detail values. |
| `PathRequiredForSubprojectScope` | Directory/file scopes require a nonblank path. |
| `AllFileChannelsUseSameScope` | Directory, key-file, and language channels share the same scoped file set. |
| `ReturnedFilesProjectScoped` | File-derived rows belong to the resolved project id. |
| `MetricRowsProjectConsistent` | Key-file metrics agree with the file's project id. |
| `TopicsRespectDetailAndProject` | Brief detail omits topics; included topics name the resolved project. |
| `EffectsProjectScoped` | Effect enrichment uses the resolved project id. |
| `LiteralDirectoryPathMatching` | Directory path filters treat `%` and `_` literally after escaping. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=768M \
      PGMCP_TLC_METASPACE=32m PGMCP_TLC_CLASS_SPACE=16m \
      PGMCP_TLC_CODE_CACHE=32m \
      ../../../scripts/tlc-capped.sh CodeSummarizeScope.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 8 distinct states, 16 generated.
All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test oracle_code_summarize \
  --test tool_scorecard_integration --build-jobs 1
```

Result: 6/6 passed. The focused run covers synthetic corpus totals,
`detail=brief`, normalized directory scoping across output channels, duplicate
project rejection, invalid filter rejection, and file-scope path requirements.
