# Community Detection Formal Verification Traceability

Status: focused graph-analysis slice for `community_detection`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `community_detection` at 3
calls, the next uncovered tool after `api_stability`. The tool is read-only: it
loads a project-scoped graph, runs Louvain community detection, and returns
community membership and modularity diagnostics.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `community_detection` | Reject blank/missing/duplicate projects; validate `graph_type`; reject non-finite resolution and clamp finite values; exclude stale `code_graph_edges` whose source/target files belong to another project; reuse the resolved project id for effect enrichment; emit numeric modularity/community fields; remain read-only with no persistent locks. | `tla/CommunityDetectionScope.tla`; `oracle_community_detection`. |

## Issues Found And Corrected

The tool resolved projects with a raw `SELECT id FROM projects WHERE name = $1`
and `fetch_optional`, rather than the shared duplicate-aware resolver.

Correction: project names are trimmed and resolved through `project_id_or_err`,
so blank, missing, and duplicate names fail closed.

Unknown `graph_type` values silently selected the combined graph. Resolution
was accepted without finite checks or bounds.

Correction: `graph_type` is closed to `import | co_change | combined`, blank
means the default `import`, and finite resolution values clamp to `0.05..=10.0`.

The edge query scoped `code_graph_edges.project_id`, but did not require joined
`indexed_files` rows to belong to that same project. A stale cross-project edge
could pull another project's file path into the graph.

Correction: source and target file joins now require `indexed_files.project_id =
code_graph_edges.project_id`; stale target rows are excluded.

The response used string `modularity_q` and `community_count`, while the oracle
expected numeric `modularity`, `num_communities`, and per-community `members`.

Correction: the response keeps the existing fields and adds numeric compatibility
fields so clients can validate the graph invariant directly.

## Formal Model

`tla/CommunityDetectionScope.tla` models valid and invalid project resolution,
closed graph types, finite resolution clamping, stale source/target edges,
same-project effect enrichment, and numeric community envelope output.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Invalid projects, graph types, and non-finite resolution reject without rows. |
| `GraphTypeClosed` | Accepted graph type is one of the closed modes. |
| `ResolutionBounded` | Accepted resolution is finite and bounded. |
| `StaleEdgesExcluded` | Cross-project file edges cannot leak into output. |
| `EffectEnrichmentUsesResolvedProject` | Effect enrichment uses the same project id as graph rows. |
| `NumericCommunityEnvelope` | Accepted responses include numeric modularity/community fields. |
| `ReadOnlyNoHeldLock` | The model has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_community_detection --build-jobs 1
```

Result: 3/3 passed for the focused community-detection oracle suite.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh CommunityDetectionScope.tla
```

Result: TLC exit 0; 7 distinct states, 14 states generated; no invariant
violations.
