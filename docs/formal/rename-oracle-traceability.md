# Rename Oracle Formal Verification Traceability

Status: focused fuzzy-adapter slice for `rename_oracle`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `rename_oracle` in the
2-call cluster. The tool is intentionally local and read-only: callers provide
the removed symbol name and the current-day candidate names, pgmcp builds a
temporary DAWG-backed Damerau-Levenshtein transducer, and a phonetic/articulatory
tiebreak chooses the likely rename.

This slice verifies pgmcp's adapter boundary. The underlying edit-distance and
dictionary invariants are inherited from `liblevenshtein-rust/` and
`libdictenstein/`; pgmcp must ensure only bounded, normalized inputs reach those
engines.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `rename_oracle` | Trim and validate symbol names; reject blank/oversized names before dictionary construction; bound candidate count and total bytes; dedupe candidates before the DAWG boundary; avoid building a dictionary for empty candidate sets; stream best-candidate selection without collecting all matches; execute read-only with no locks. | `tla/RenameOracleBounds.tla`; `oracle_rename_oracle` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool previously built a `DynamicDawgChar` directly from caller-provided
strings. There was no cap on candidate count, per-name length, or total input
bytes, and blank names were accepted into the dictionary boundary. It also
collected every edit-distance match into a vector before selecting the best
candidate.

The wrapper now trims names, rejects blank or oversized names, caps the raw
candidate list at 5,000 entries, caps the total normalized input at 1 MiB,
deduplicates candidates with deterministic ordering, returns `null` without
building a dictionary for an empty candidate set, and uses iterator `min_by`
directly so best-candidate selection keeps only the current best candidate.

## Formal Model

`tla/RenameOracleBounds.tla` models the adapter boundary around the inherited
DAWG/transducer engine. It abstracts individual candidate strings to the finite
properties relevant for safety: blankness, byte sizes, raw candidate count,
deduplicated count, and whether a match exists.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidInputsDoNotBuildDictionary` | Invalid removed/candidate names and oversized inputs reject before DAWG/transducer construction. |
| `CandidateCountBounded` | Accepted responses expose at most the configured candidate bound. |
| `EmptyCandidatesAvoidDictionary` | Empty candidate sets return no match without building a dictionary. |
| `DedupedCountUsed` | Reported candidate count is the normalized, deduplicated count. |
| `StreamingBestSelection` | Best-candidate selection keeps at most one candidate in memory. |
| `NormalizedSuccess` | Successful calls use normalized names at the fuzzy boundary. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh RenameOracleBounds.tla
```

Result: 10 distinct states, 19 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_rename_oracle --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.

## Inherited Proof Surface

`rename_oracle` relies on the same fuzzy-search substrate as the earlier fuzzy
slices. The relevant external obligations remain in:

| Project | Inherited evidence used here |
| --- | --- |
| `liblevenshtein-rust/` | Transducer query soundness, distance cross-validation, bounded phonetic normalization, and value-yielding query parity. |
| `libdictenstein/` | DAWG/ARTrie dictionary construction and query invariants, including non-blocking trie behavior. |
