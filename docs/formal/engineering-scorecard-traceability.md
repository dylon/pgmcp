# Engineering Scorecard Formal Traceability

## Scope

This slice covers `engineering_scorecard` and the `quality-history` cron path
that reuses the scorecard through `quality_report`.

## Verified Properties

- Public scorecard requests trim project names, reject blanks, and reject
  duplicate display names.
- The dependency-health and architecture acyclicity metrics count files in any
  import-cycle SCC, not only two-node reciprocal pairs.
- The `no_god_files` ORR gate is scoped by the resolved project id.
- The quality-history cron snapshots the concrete ids from `list_projects`
  instead of re-resolving duplicate display names.
- The scorecard path is read-only and does not acquire runtime locks; the cron
  writes exactly one history row per successful project snapshot.
- The cron can skip finding collectors and mark finding-backed dimensions N/A,
  bounding memory without presenting skipped findings as clean findings.

## Implementation Links

- `src/mcp/tools/tool_engineering_scorecard.rs`
- `src/mcp/tools/sota_helpers.rs::import_cycle_file_count`
- `src/mcp/tools/tool_architecture_quality.rs`
- `src/quality/aggregate.rs::aggregate_for_project`
- `src/cron/quality_history.rs`
- `pgmcp-testing/tests/oracle_engineering_scorecard.rs`
- `pgmcp-testing/tests/quality_history_cron_registered.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/EngineeringScorecardScope.tla`
- Config: `docs/formal/tla/EngineeringScorecardScope.cfg`
- Focused Rust regressions:
  - `cargo nextest run -p pgmcp-testing --test oracle_engineering_scorecard --build-jobs 1`
  - `cargo nextest run -p pgmcp-testing --test quality_history_cron_registered --build-jobs 1`

## Concurrency And Memory Notes

`import_cycle_file_count` loads the materialized import graph once and runs
Tarjan SCC in memory, O(files + edges). It does not add locks or shared mutable
state. The quality-history cron remains serialized by the scheduler's existing
cron lock, and the aggregate path now honors `compute_findings = false` to avoid
retaining full finding payloads during history snapshots.
