# Dependency Graph Formal Verification Traceability

Status: focused high-use graph slice for `dependency_graph`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed `dependency_graph` at 5 calls,
making it the next uncovered tool after the 6-call memory/architecture cluster.
The tool is read-only, so the verification target is request validation,
project/file scoping, bounded rendering, and deterministic focus-file behavior.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `dependency_graph` | Normalize project/focus inputs; reject blank or duplicate project names; validate format and edge types; clamp focus depth; resolve focus files exactly and fail closed when missing; return only edges whose graph row, source file, and target file all belong to the resolved project; bound edges/DOT rendering while preserving full counts; keep cross-project dependency metadata sourced from the resolved project id. | `tla/DependencyGraphScope.tla`; `oracle_dependency_graph`. |

## Issues Found And Corrected

The tool looked up projects by display name with an unconstrained
`SELECT id FROM projects WHERE name = $1` and used the first matching row. That
could report an arbitrary project when duplicate display names existed.

Correction: project lookup now uses the shared `project_id_or_err` helper, which
trims the requested name and fails closed when the display name is blank,
missing, or non-unique.

The edge query filtered only on `code_graph_edges.project_id`. It did not require
the source and target files to belong to the same resolved project id, so stale
cross-project file ids could leak into the graph.

Correction: the SQL now requires `sf.project_id = e.project_id`, requires any
non-null target file to join with the same project id, and filters edge types in
SQL.

The `focus_file` path used substring matching over already-visible nodes and
fell back to the full graph when no match was found. Focus resolution is now an
exact `indexed_files.relative_path` lookup inside the resolved project; missing
focus files reject instead of returning an unexpectedly broad graph.

## Formal Model

`tla/DependencyGraphScope.tla` models valid and invalid requests over duplicate
projects, invalid formats, invalid edge types, missing focus files, bounded
depths, valid same-project graph rows, and stale cross-project edges.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsRejectNoRows` | Invalid requests reject and return no graph rows. |
| `DuplicateProjectsReject` | Non-unique display names fail closed. |
| `FocusMissingRejects` | A missing focus file cannot fall back to the full graph. |
| `EdgesStayInResolvedProject` | Returned rows agree with the resolved project id for graph row, source file, and target file. |
| `OnlyAllowedEdgeTypesReturned` | Returned edges have validated edge kinds. |
| `DepthBounded` | Negative and oversized focus depths normalize into the finite supported range. |
| `ReportedEdgesBounded` | Rendered edge output respects the configured cap. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh DependencyGraphScope.tla
```

Result: 27,399 distinct states, 27,399 generated states, no invariant
violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_dependency_graph --build-jobs 1
```

Result: 3/3 passed, including normalized request validation, stale-edge
rejection, duplicate-project fail-closed behavior, and expected graph counts.
