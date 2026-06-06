# Doc Coverage Gaps Formal Verification Traceability

Status: focused high-use documentation-coverage slice for `doc_coverage_gaps`.

## Scope

The refreshed 31-day `mcp_tool_calls` snapshot showed `doc_coverage_gaps` at 7
calls with no non-ok outcomes. This slice covers the MCP request boundary,
project identity, topic-row classification, and read-only enrichment.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `doc_coverage_gaps` | Trim and reject blank project names; fail closed on duplicate project display names; query topic rows by resolved project id in production; keep unknown projects as an empty/guidance response; classify doc coverage by the documented threshold table; scope effect enrichment to the same project id; remain read-only and lock-free. | `tla/DocCoverageGapsScope.tla`; `pgmcp-testing/tests/oracle_doc_coverage_gaps.rs`. |

## Issues Found And Corrected

`doc_coverage_gaps` previously passed the raw project string into the legacy
name-scoped query. Duplicate project display names could therefore merge or
arbitrarily select project data, and effect enrichment used a separate
`SELECT id FROM projects WHERE name = $1` path.

Correction: production calls now trim the project, reject blank names, resolve
at most one project id, query documentation topic coverage by `indexed_files`
`project_id`, and use the same resolved id for effect enrichment. The mock path
still calls the existing trait method so the pure oracle tests remain cheap.

The response now echoes the normalized project string.

## Formal Model

`tla/DocCoverageGapsScope.tla` models blank, duplicate, missing, trimmed, and
normal requests over a small topic universe.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankProjectsRejected` | Empty/whitespace projects reject before query. |
| `DuplicateProjectsRejected` | Duplicate display names reject with no topics or effects. |
| `MissingProjectsHaveNoScopedData` | Unknown projects do not return topic/effect rows. |
| `TopicRowsProjectScoped` | Returned topic rows all match the resolved project id. |
| `EffectsUseResolvedProject` | Effect enrichment uses the same project id. |
| `StatusClassificationCorrect` | Status labels match the `>0.30`, `>0.05`, else threshold table. |
| `ProjectOutputNormalized` | The output project is the trimmed request project. |
| `ReadOnlyNoLocks` | The tool writes nothing and acquires no locks. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh DocCoverageGapsScope.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 6 distinct states, 12 generated.
All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test oracle_doc_coverage_gaps --build-jobs 1
```

Result: 5/5 passed. The run covers threshold classification, worst-first
sorting, blank-project rejection, duplicate project display-name rejection, and
empty-topic guidance.
