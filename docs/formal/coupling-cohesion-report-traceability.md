# Coupling Cohesion Report Formal Traceability

Status: focused high-use architecture slice for `coupling_cohesion_report`.

## Scope

This slice covers `coupling_cohesion_report`: project and sort-mode
normalization, module-depth bounds, duplicate-name fail-closed behavior,
project-id scoped import graph construction, stale cross-project edge rejection,
bounded module output, same-project effect enrichment, and read-only execution.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `module_depth` is clamped to `1..=8` before graph grouping.
- `sort_by` is normalized and restricted to `distance | instability | coupling
  | cohesion`.
- Import edges enter module metrics only when both endpoints belong to the
  resolved project id.
- Stale cross-project edges cannot introduce foreign modules or coupling counts.
- The response is capped at 2000 modules and exposes the uncapped total plus a
  `truncated` flag.
- Effect enrichment uses the same resolved project id as graph analysis.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_coupling_cohesion_report.rs`
- `pgmcp-testing/tests/oracle_coupling_cohesion_report.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/CouplingCohesionScope.tla`
- Config: `docs/formal/tla/CouplingCohesionScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_coupling_cohesion_report --build-jobs 1`

## Concurrency Notes

The implementation performs read-only SQL and builds an in-memory graph from a
single scoped result set. It introduces no locks, channels, joins, or background
workers.
