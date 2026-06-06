# Find Orphans Formal Traceability

Status: focused high-use topic/quality slice for `find_orphans`.

## Scope

This slice covers `find_orphans`: optional project normalization, blank supplied
project rejection, detail validation, bounded limits, project-id scoped orphan
rows, language filtering for both chunk and file detail modes, same-project
effect enrichment, and read-only execution.

## Verified Properties

- Omitted project means all projects; a supplied blank project fails closed.
- Duplicate project display names fail closed when a project filter is supplied.
- `detail` is restricted to `files | chunks`.
- `limit` is clamped to `1..=1000` and bounds both chunk and file outputs.
- Real-DB paths use a resolved `projects.id`, not a display-name predicate.
- Language filters apply to both file summaries and chunk rows.
- Effect enrichment uses the same resolved project id when a project is supplied.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_find_orphans.rs`
- `src/db/queries/topics.rs::find_orphan_chunks_by_project_id`
- `src/db/queries/topics.rs::find_orphan_file_summary_by_project_id`
- `pgmcp-testing/tests/oracle_find_orphans.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/FindOrphansScope.tla`
- Config: `docs/formal/tla/FindOrphansScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_find_orphans --build-jobs 1`

## Concurrency Notes

The implementation performs read-only SQL after request normalization. It does
not spawn tasks, acquire locks, or mutate shared state beyond relaxed stats
counters.
