# Find Misplaced Code Formal Traceability

## Scope

This slice covers `find_misplaced_code`, including request normalization,
project scoping, mismatch threshold bounds, and effect enrichment.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `min_mismatch` is finite and clamped to `0.0..=1.0`.
- Production topic rows are loaded by resolved `project_id`, not by display
  name.
- Effect enrichment reuses the same resolved `project_id`.
- Single-file directories are suppressed because there is no meaningful
  directory majority.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_find_misplaced_code.rs`
- `src/db/queries/topics.rs::load_chunk_topic_assignments_for_files_by_project_id`
- `pgmcp-testing/tests/oracle_find_misplaced_code.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/FindMisplacedCodeScope.tla`
- Config: `docs/formal/tla/FindMisplacedCodeScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_find_misplaced_code --build-jobs 1`

## Concurrency Notes

The tool performs read-only SQL plus in-memory grouping. The by-project-id query
uses a transaction only for local statement timeout/application-name settings;
it does not take advisory locks or mutate database rows.
