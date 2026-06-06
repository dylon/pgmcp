# Fuzzy Grep Adapter Formal Verification Traceability

Status: focused fuzzy-search adapter slice for `fuzzy_grep`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot showed `fuzzy_grep` at 5 calls. The
tool is read-only and scans caller-supplied haystacks through liblevenshtein's
`TokenGrep`, so the local pgmcp obligations are adapter bounds and stable
delegation rather than re-proving edit-distance semantics.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `fuzzy_grep` | Reject blank or oversized requests before scanning; clamp the default edit-distance budget before converting to `u8`; reject explicit per-token `:N` budgets above pgmcp's cap; bound haystack documents, total bytes, and reported matches; remain read-only except for request stats. | `tla/FuzzyGrepAdapterBounds.tla`; `g10_fuzzy_grep_tools`. |

## Issues Found And Corrected

The tool previously cast `max_distance: u32` directly to `u8`. Large caller
values could wrap before reaching `TokenGrep`.

Correction: the adapter clamps the caller value to pgmcp's finite cap before the
`u8` conversion, and validates explicit token-distance suffixes in the query so
they cannot bypass that cap.

The tool also accepted unbounded query/haystack sizes and collected every match
from every document into the response.

Correction: query bytes, document count, per-document bytes, total haystack
bytes, and reported matches are bounded. The response reports
`matches_truncated` when the match cap is reached.

## Formal Model

`tla/FuzzyGrepAdapterBounds.tla` models accepted and rejected request shapes,
large raw distances, explicit per-token distance rejection, and finite match
reporting.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsDoNotScan` | Invalid request shapes do not invoke the scanner. |
| `ScansOnlyValidRequests` | Every scan comes from a request that passed all adapter guards. |
| `DistanceNeverWrapsOrExceedsCap` | The reported/default distance is the clamped value, never a wrapped `u8`. |
| `ReportedMatchesBounded` | The response cannot exceed pgmcp's match cap. |
| `TruncationSound` | Truncation metadata matches the candidate-count boundary. |
| `ReadOnlyAdapter` | The model has no persistent-state mutation path. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh FuzzyGrepAdapterBounds.tla
```

Result: exit 0; no invariant violations; 1,957 distinct states and 1,957
states generated at depth 7.

```bash
cargo nextest run -p pgmcp-testing --test g10_fuzzy_grep_tools --build-jobs 1
```

Result: 5/5 passed, covering positional matches, non-wrapping distance clamp,
explicit token-budget rejection, and output truncation.
