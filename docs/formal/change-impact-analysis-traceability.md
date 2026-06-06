# Change Impact Analysis Formal Verification Traceability

Status: focused graph/change-analysis slice for `change_impact_analysis`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows
`change_impact_analysis` at 2 calls. The tool resolves one project and target
file, then merges reverse import reachability, co-change coupling, optional
semantic similarity, resolved caller reachability, project effect counts, and
project-level cross-project dependents.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `change_impact_analysis` | Reject blank files and missing/duplicate projects; clamp BFS depth; keep import, co-change, semantic, resolved-caller, and effect-count channels scoped to the resolved project; allow only `cross_project_dependents` to intentionally report other projects; remain read-only with no persistent locks. | `tla/ChangeImpactScope.tla`; `oracle_change_impact_analysis`; filtered `tool_graph_integration`. |

## Issues Found And Corrected

Project lookup used `SELECT id FROM projects WHERE name = $1` directly. That did
not fail closed for duplicate display names and did not trim the project input.

Correction: the tool now uses the shared `project_id_or_err` resolver and trims
`project`; duplicate names reject with `invalid_params`.

The file input and BFS depth were not normalized. Blank files could reach the
database lookup, and very large or non-positive depths could make traversal
behavior surprising.

Correction: `file` is trimmed and must be non-empty; `depth` is clamped to
`1..=12`.

Reverse import and resolved-caller traversal joined through file ids but did not
require reached source files to belong to the resolved project. Corrupt or
cross-project graph/reference rows could therefore enter the file-level impact
list.

Correction: reverse import queries, resolved-caller BFS, and reached-file
resolution now filter joined `indexed_files.project_id` to the resolved
project. The intentional cross-project channel remains only
`cross_project_dependents`.

## Formal Model

`tla/ChangeImpactScope.tla` models fail-closed project/file validation, depth
clamping, same-project scoping for import/co-change/semantic/resolved-caller and
effect-count channels, intentional project-dependent output, and read-only
execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectOrFileRejects` | Missing/duplicate projects or invalid files reject before impact channels run. |
| `DepthClamped` | Effective BFS depth stays in `1..=12`. |
| `ImportRowsProjectScoped` | Reverse import rows are same-project only. |
| `CochangeRowsProjectScoped` | Co-change rows resolve back into the same project. |
| `SemanticRowsProjectScopedAndOptional` | Semantic rows are optional and same-project only. |
| `ResolvedCallerRowsProjectScoped` | Reverse resolved-call traversal cannot add another project's files. |
| `EffectBreakdownProjectScoped` | Effect counts do not include another project's symbol effects. |
| `OnlyProjectDependentsMayCrossProject` | Cross-project output is confined to `cross_project_dependents`. |
| `ReadOnlyNoLock` | The tool has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_change_impact_analysis --build-jobs 1
```

Result: 3/3 passed for direct dependents, duplicate-project rejection, input
normalization/depth clamp, and cross-project row suppression.

```bash
cargo nextest run -p pgmcp-testing --test tool_graph_integration \
  change_impact_analysis_runs_against_real_db --build-jobs 1
```

Result: 1/1 passed for the existing graph integration smoke path.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh ChangeImpactScope.tla
```

Result: TLC exit 0; 6 distinct states, 12 states generated; no invariant
violations.
