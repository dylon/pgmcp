# Phonetic Symbol Search Formal Verification Traceability

Status: focused fuzzy/phonetic retrieval slice for `phonetic_symbol_search`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `phonetic_symbol_search` in the
2-call cluster. Earlier fuzzy verification already covered shared
distance/limit clamping for `fuzzy_symbol_search`, `fuzzy_path_search`, and
`phonetic_symbol_search`; this slice tightens the `phonetic_symbol_search`
request boundary and records the dependency contracts it relies on.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `phonetic_symbol_search` | Trim and reject blank queries before opening the project trie; reject oversized queries; trim and reject blank project names; clamp edit distance/result limit; search only the requested project's persistent symbol trie; use the same normalized project for per-project phonetic rules; report only matches within the effective phonetic edit distance. | `tla/PhoneticSymbolSearchBoundary.tla`; `phonetic_search_and_correct_query`. |

## Issues Found And Corrected

The tool already used `bounded_max_distance` and `bounded_limit`, and
`open_symbol_trie` trimmed project names before resolving project IDs. However,
the tool passed the raw query to phonetic search and articulatory scoring, used
the raw project string for phonetics lookup and response reporting, and did not
reject blank or oversized queries at the MCP boundary.

Correction: `tool_phonetic_symbol_search` now trims `query` and `project`,
rejects blank/over-512-byte queries before opening the trie, rejects blank
projects before trie lookup, uses the normalized project for both the trie and
per-project phonetics lookup, and returns the effective `limit` alongside the
existing `max_distance`.

## Inherited Contracts

This pgmcp slice intentionally does not modify `libdictenstein` or
`liblevenshtein-rust`. It relies on their existing formal corpora:

| Dependency | Imported guarantee used here |
| --- | --- |
| `liblevenshtein-rust` | Phonetic rewrite-rule well-formedness, value-yielding query soundness, and phonetic normalized dictionary behavior documented under `docs/verification/`. |
| `libdictenstein` | Persistent trie/map publication and lock-free ARTrie traversal contracts documented under `formal-verification/`. |
| `lling-llang` | WFST/path/language and lazy-composition proof context for phonetic/edit composition. |
| `libgrammstein` | Dependency bridge checks tying grammar/query wrappers to the same fuzzy backend contracts. |

## Formal Model

`tla/PhoneticSymbolSearchBoundary.tla` models invalid and valid request paths
for the MCP wrapper. The dependency search is abstracted as a project-local
vocabulary whose rows carry phonetic edit distances.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankQueriesRejected` | Empty or whitespace queries are rejected before trie search. |
| `OversizedQueriesRejected` | Queries over the wrapper byte cap are rejected before trie search. |
| `BlankProjectsRejected` | Empty or whitespace project names are rejected. |
| `EffectiveDistanceClamped` / `EffectiveLimitClamped` | Caller-supplied bounds are normalized to the shared fuzzy limits. |
| `RowsProjectScoped` | Returned rows belong only to the normalized requested project. |
| `RowsWithinEffectiveDistance` | Returned rows are within the effective phonetic edit distance. |
| `OutputWithinLimit` | Result count never exceeds the effective limit. |
| `ExactModeDoesNotAdmitTypos` | `max_distance=0` admits only exact normalized-distance rows. |
| `NormalizedInputsUsed` | Query/project values used at the boundary are trimmed values. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh PhoneticSymbolSearchBoundary.tla
```

Result: 14 distinct states, 22 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test phonetic_search_and_correct_query --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
