# Find Coupled Files Formal Verification Traceability

Status: focused high-use co-change analysis slice for `find_coupled_files`.

## Scope

`find_coupled_files` was the next telemetry-ranked tool without a formal ledger
row after the ontology authoring slice. It is also consumed by several
recommendation tools, so its project scoping and parameter bounds matter beyond
the direct MCP surface.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `find_coupled_files` | Trim and reject blank projects; reject unknown or duplicate project names in production; reject non-finite `min_coupling`; clamp `min_coupling` to 0..1, `min_commits` to 1..10000, and `limit` to 1..200; check git-history presence by resolved project id; query co-change rows by resolved project id rather than display name; preserve bulk-commit exclusion; return rows satisfying threshold/min-commit filters and result limit; remain read-only and lock-free. | `tla/FindCoupledFilesScope.tla`; `pgmcp-testing/tests/oracle_find_coupled_files.rs`. |

## Issues Found And Corrected

The MCP wrapper used raw request parameters. Negative limits bypassed truncation
after casting to `usize`, negative `min_commits` admitted every pair, and
out-of-range coupling thresholds were not normalized. Correction: the wrapper
now validates finite coupling thresholds and clamps all bounded numeric
parameters before any query.

The production query scoped by `projects.name`. Duplicate display names could
merge unrelated git histories into one co-change result set. Correction: the
tool now resolves the display name to exactly one `project_id`, rejects duplicate
names, checks git file data by that id, and calls a project-id-scoped SQL helper.
The older name-based query remains for existing trait callers and mocks.

The effect enrichment path did a separate non-unique project-name lookup.
Correction: the production path uses the same strict project resolver and
degrades to an empty enrichment only if enrichment itself fails.

## Concurrency Boundary

`find_coupled_files` is read-only. It performs bounded SELECTs and does not
acquire advisory locks, row locks, process mutexes, or background worker state.
The only concurrency-sensitive boundary is scoping: every production row query
uses the resolved immutable `project_id`, so concurrent insertion of another
project with the same display name cannot be silently merged into a successful
request.

## Formal Model

`tla/FindCoupledFilesScope.tla` models representative valid, blank, duplicate,
unknown, no-git-data, non-finite coupling, negative-bound, and >1.0 coupling
requests.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Blank/unknown/duplicate projects and non-finite coupling reject. |
| `SuccessfulRequestsResolvedUniqueProject` | Successful requests use a single resolved project id. |
| `ParamsBounded` | Successful/no-data requests carry bounded normalized parameters. |
| `CandidateRowsProjectScoped` | Candidate co-change rows belong to the resolved project id. |
| `BulkCommitsExcluded` | Bulk-commit pairs are not eligible output rows. |
| `ThresholdsEnforced` | Eligible rows satisfy normalized Jaccard and min-commit thresholds. |
| `LimitBound` | Result counts never exceed the normalized limit. |
| `NoDataDoesNotQueryRows` | No-git-data responses return no candidate rows. |
| `ReadOnlyNoLocks` | The tool writes nothing and holds no locks. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh FindCoupledFilesScope.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 8 distinct states, 16
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test oracle_find_coupled_files --build-jobs 1
```

Result: 4/4 passed. The focused run covers planted Jaccard pairs,
uncoupled-file exclusion, project/parameter normalization, result-limit
clamping, and duplicate project display-name rejection.
