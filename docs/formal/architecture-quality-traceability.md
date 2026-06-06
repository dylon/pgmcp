# Architecture Quality Formal Traceability

Status: focused high-use architecture slice for `architecture_quality`.

## Scope

This slice covers `architecture_quality`: request normalization, detail-mode
validation, duplicate-name fail-closed behavior, project-id-scoped metric
inputs, stale cross-project import-edge rejection, N/A-dimension mean
semantics, and read-only execution.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `detail` is normalized and restricted to `summary | full`.
- `full` responses include per-dimension descriptions; `summary` keeps the
  compact score envelope.
- `file_metrics` rows are accepted only when the metric project id agrees with
  the owning `indexed_files.project_id`.
- SDP edge scoring excludes stale import edges whose source or target file no
  longer belongs to the resolved project id.
- Data-absent dimensions render as `N/A` and are excluded from the overall
  score denominator.
- The tool is read-only, takes no runtime locks, and does not spawn workers.

## Implementation Links

- `src/mcp/tools/tool_architecture_quality.rs`
- `src/mcp/tools/sota_helpers.rs::import_cycle_file_count`
- `pgmcp-testing/tests/oracle_architecture_quality.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/ArchitectureQualityScope.tla`
- Config: `docs/formal/tla/ArchitectureQualityScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_architecture_quality --build-jobs 1`

## Concurrency Notes

The implementation performs bounded read-only SQL plus an in-memory Tarjan SCC
over import edges for the acyclicity dimension. It introduces no locks, no
background threads, and no shared mutable state beyond relaxed stats counters.
Concurrent indexing may change later calls, but a single response cannot mix in
rows from a different project id.
