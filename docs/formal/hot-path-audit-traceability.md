# Hot Path Audit Formal Traceability

Status: focused high-use workflow slice for `hot_path_audit`.

## Scope

This slice covers `hot_path_audit`: project normalization, finite percentile
threshold validation, bounded thresholds and limits, duplicate-name fail-closed
behavior, project-id scoped metric rows, stale cross-project metric rejection,
same-project effect-symbol enrichment, and read-only execution.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `percentile_threshold` must be finite and is clamped to `0.0..=1.0`.
- `limit` is clamped to `1..=1000`.
- Hot-path metrics are accepted only when `file_metrics.project_id` agrees with
  the owning `indexed_files.project_id`.
- Effect-symbol enrichment uses the same resolved project id as the hot-path
  metric query.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_hot_path_audit.rs`
- `src/db/queries/metrics.rs::find_hot_paths_by_project_id`
- `pgmcp-testing/tests/oracle_hot_path_audit.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/HotPathAuditScope.tla`
- Config: `docs/formal/tla/HotPathAuditScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_hot_path_audit --build-jobs 1`

## Concurrency Notes

The implementation performs read-only SQL plus in-memory classification of the
bounded result set. It introduces no locks, channels, joins, or background
workers.
