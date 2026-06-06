# Compare Files Formal Verification Traceability

Status: focused high-use similarity slice for `compare_files`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`compare_files` at 20 calls. The tool resolves two file references and reports a
one-to-one greedy alignment of similar chunks.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `compare_files` | `project:relative_path` references fail closed when project display names are ambiguous; resolved chunk rows belong to the two resolved files; chunk alignment is one-to-one over A and B chunks. | `tla/CompareFilesResolution.tla`; `queries_resolve_file_reference_rejects_ambiguous_project_name`; `oracle_similarity_tools`. |

## Issue Found And Corrected

`resolve_file_reference` used `WHERE p.name = $1 AND f.relative_path = $2` for
`project:relative_path` references. If multiple indexed projects shared the
same display name, the resolver could return an arbitrary matching file and
`compare_files` could compare against the wrong project.

Correction: `resolve_file_reference` now resolves project-qualified references
through a `unique_project` CTE. Duplicate display names produce no file
reference, so callers fail closed with the existing not-found envelope.

## Formal Model

`tla/CompareFilesResolution.tla` models unique project references, duplicate
project display names, absolute references, resolved files, chunks, and a
one-to-one alignment subset.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `AmbiguousProjectReferencesRejected` | Duplicate display-name references never produce comparison rows. |
| `RowsBelongToResolvedFiles` | Every aligned chunk belongs to the resolved A or B file respectively. |
| `OneToOneAlignment` | No aligned chunk from either file is used more than once. |
| `ResolvedOrRejected` | Successful responses always have both file references resolved. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-CompareFilesResolution \
  -config docs/formal/tla/CompareFilesResolution.cfg \
  docs/formal/tla/CompareFilesResolution.tla
```

Result: 1,030 distinct states, 1,030 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test query_smoke_queries \
  queries_resolve_file_reference_rejects_ambiguous_project_name --build-jobs 1
```

Result: 1/1 passed.

```bash
cargo nextest run -p pgmcp-testing --test oracle_similarity_tools --build-jobs 1
```

Result: 5/5 passed, including the existing `compare_files` known-score oracle.
