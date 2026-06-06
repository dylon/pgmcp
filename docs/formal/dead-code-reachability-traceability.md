# Dead Code Reachability Formal Verification Traceability

Status: focused call-graph reachability slice for `dead_code_reachability`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `dead_code_reachability` at
3 calls. The tool is read-only: it resolves a project, chooses public/entry
roots, walks symbol call edges, and reports in-project symbols not reached from
those roots.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `dead_code_reachability` | Reject blank/missing/duplicate projects; normalize output bounds; exclude test roots unless requested; include only exact call-edge resolutions by default and bare-name edges only on opt-in; reject stale cross-project source/target symbol edges; reuse the resolved project id for effect enrichment; terminate traversal on a finite visited set; remain read-only with no persistent locks. | `tla/DeadCodeReachabilityScope.tla`; `oracle_dead_code_reachability`. |

## Issues Found And Corrected

The tool used the duplicate-aware project resolver, but did not normalize the
project string for response output or effect enrichment.

Correction: project names are trimmed once, and both response output and effect
enrichment reuse the resolved project id.

The caller-provided `limit` was not upper-bounded. Negative and zero values also
had surprising behavior because the loop pushed a candidate before checking the
effective bound.

Correction: `limit` now normalizes to `1..=1000` before enumeration and is
returned in the response envelope.

`include_tests=false` excluded test files from reported candidates, but test
symbols could still become roots and mark production helpers reachable.

Correction: the same test-file predicate now scopes both roots and candidate
enumeration.

When `include_bare_name=true`, the edge query admitted every resolved call edge
with a target symbol, not just exact plus `bare_name_in_project`.

Correction: traversal accepts `exact_in_file | exact_via_import` by default and
adds only `bare_name_in_project` when explicitly requested.

The call-edge query scoped rows through `symbol_references.source_file_id`, but
did not verify that `source_symbol_id` and `target_symbol_id` also belonged to
that resolved project. Stale rows could walk through another project's symbol
and inflate reachability.

Correction: traversal joins both source and target `file_symbols` rows and their
`indexed_files` rows, requiring all of them to belong to the resolved project.

## Formal Model

`tla/DeadCodeReachabilityScope.tla` models invalid project modes, no-symbol
soft-fail behavior, finite limit normalization, test-root inclusion policy,
exact/bare-name edge policies, stale source/target edges, bounded BFS closure,
same-project effect enrichment, and read-only/no-lock execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectsReject` | Blank, missing, and duplicate project names reject without roots or candidates. |
| `NoSymbolsSoftFail` | Missing symbol data returns the explicit soft-fail envelope without candidates. |
| `LimitBounded` | Accepted non-rejected responses expose a bounded finite limit. |
| `TestRootsOptIn` | Test roots affect reachability only when `include_tests=true`. |
| `BareNameOptIn` | Bare-name edges affect reachability only when `include_bare_name=true`. |
| `AcceptedEdgesSameProject` | Traversed edges have source/target symbols and files in the resolved project. |
| `AcceptedEdgesClosedKind` | Traversed edge kinds are exact or explicitly opted-in bare-name edges. |
| `StaleEdgesExcluded` | Cross-project symbols cannot become reached or reported candidates. |
| `DeadCandidatesProjectScoped` | Dead candidates are drawn only from scoped project symbols. |
| `EffectEnrichmentUsesResolvedProject` | Effect enrichment uses the same project id as traversal. |
| `BoundedTraversal` | The modeled BFS closure is finite and bounded by the symbol set. |
| `ReadOnlyNoHeldLock` | The model has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_dead_code_reachability --build-jobs 1
```

Result: 2/2 passed for the focused dead-code reachability oracle suite.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh DeadCodeReachabilityScope.tla
```

Result: TLC exit 0; 6 distinct states, 12 states generated; no invariant
violations.
