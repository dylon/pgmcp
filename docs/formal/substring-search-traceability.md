# Substring Search Formal Verification Traceability

Status: focused in-memory fuzzy-adapter slice for `substring_search`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `substring_search` in the
2-call cluster. This tool is an exact, case-sensitive, caller-supplied
haystack search backed by `libdictenstein`'s suffix automaton.

This slice verifies pgmcp's adapter boundary: exact semantics are preserved,
but unbounded caller data is rejected before suffix-automaton construction.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `substring_search` | Reject empty/oversized needles and terms; cap haystack count and total bytes; dedupe haystack terms before index construction; avoid index construction for empty haystacks; preserve exact case-sensitive membership semantics; execute read-only. | `tla/SubstringSearchBounds.tla`; `oracle_substring_search` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool previously built a suffix automaton directly from caller-provided
haystacks with no bound on term count, per-term size, or total input bytes.
It also rebuilt work for duplicate haystack terms.

The wrapper now bounds the needle, haystack count, per-term bytes, and total
bytes before building the index. It preserves exact string content rather than
trimming terms, deduplicates terms with deterministic ordering, reports the
deduped haystack size, and returns `false` without constructing an index when
the accepted haystack is empty.

## Formal Model

`tla/SubstringSearchBounds.tla` models the pgmcp adapter boundary around the
inherited suffix-automaton engine.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidInputsDoNotBuildIndex` | Invalid or oversized inputs reject before suffix-automaton construction. |
| `AcceptedSizeIsDedupedAndBounded` | Reported haystack size is the deduped count and remains bounded. |
| `EmptyHaystackAvoidsIndex` | Empty accepted haystacks return `false` without building an index. |
| `ExactMembershipPreserved` | Accepted calls preserve exact case-sensitive substring membership. |
| `ReadOnly` | The tool performs no writes. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh SubstringSearchBounds.tla
```

Result: 10 distinct states, 19 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_substring_search --build-jobs 1
```

Result: pending until sibling `libgrammstein` builds successfully.

## Inherited Proof Surface

`substring_search` relies on `libdictenstein` suffix-automaton exact substring
membership. The pgmcp proof here is an adapter proof: bounded normalized inputs
reach the dependency, and pgmcp neither invents matches nor mutates state.
