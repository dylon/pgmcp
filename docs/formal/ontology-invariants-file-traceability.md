# Ontology Invariants For File Formal Verification Traceability

Status: focused read-side ontology slice for `ontology_invariants_for_file`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking showed
`ontology_invariants_for_file` at 4 calls, one of the next uncovered tools
after `panic_paths`. The tool is read-only: it resolves one indexed file from a
caller-supplied path and surfaces invariant concepts anchored to that file.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `ontology_invariants_for_file` | Trim and reject blank file input; treat wildcard characters literally; fail closed on ambiguous path matches; return an empty response for missing files; surface invariants only for the single resolved file; remain read-only with no persistent locks. | `tla/OntologyInvariantsFileScope.tla`; `oracle_ontology_tools`. |

## Issues Found And Corrected

The file resolver used `path LIKE '%/' || $1` with caller input and returned
the first match by id. This allowed `%` and `_` to behave as SQL wildcards and
made duplicate relative paths across projects resolve nondeterministically.

Correction: the resolver now trims input, rejects blank file strings, matches
suffixes with literal string comparison instead of `LIKE`, fetches up to two
candidate ids, and fails closed when more than one indexed file matches.

The response previously echoed the raw file string.

Correction: responses now report the normalized trimmed file string.

## Formal Model

`tla/OntologyInvariantsFileScope.tla` models exact-path resolution,
unique-relative resolution, duplicate-relative ambiguity, blank input, literal
wildcard input, and missing files.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Blank and ambiguous file requests reject without surfacing invariants. |
| `MissingFilesReturnEmpty` | Missing files return an accepted empty response. |
| `WildcardsAreLiteral` | `%` is treated as a literal file path, not a wildcard. |
| `ResolvedFileIsUnique` | A nonzero resolved file id has exactly one matching indexed file. |
| `InvariantsScopedToResolvedFile` | Every surfaced invariant is anchored to the resolved file id. |
| `ReadOnlyNoHeldLock` | The model has no persistent write or held-lock path. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh OntologyInvariantsFileScope.tla
```

Result: exit 0; no invariant violations; 6 distinct states and 12 states
generated at depth 1.

```bash
cargo nextest run -p pgmcp-testing --test oracle_ontology_tools invariants_for_file --build-jobs 1
```

Result: 3/3 passed for the filtered invariant-file tool slice.
