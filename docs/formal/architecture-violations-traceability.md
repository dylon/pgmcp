# Architecture Violations Formal Traceability

Status: focused high-use architecture slice for `architecture_violations`.

## Scope

This slice covers `architecture_violations`: normalized project/severity
inputs, duplicate-name fail-closed behavior, project-id scoped import graph
construction, stale cross-project edge rejection, bounded reported violations,
same-project effect enrichment, and read-only execution.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `severity_threshold` is normalized and restricted to `low | medium | high |
  critical`.
- Import edges enter the analysis graph only when both endpoints belong to the
  resolved project id.
- Stale cross-project edges cannot create cycles, bidirectional dependency
  reports, SDP reports, or reflexion-model divergences.
- The response is capped at 500 reported violations and exposes the uncapped
  total plus a `truncated` flag.
- Effect enrichment uses the same resolved project id as graph analysis.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_architecture_violations.rs`
- `pgmcp-testing/tests/oracle_architecture_violations.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/ArchitectureViolationsScope.tla`
- Config: `docs/formal/tla/ArchitectureViolationsScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_architecture_violations --build-jobs 1`

## Concurrency Notes

The implementation performs a bounded read-only SQL snapshot, builds an
in-memory graph from those rows, and computes violations without shared mutable
state. It adds no locks, thread joins, channels, or background workers.
