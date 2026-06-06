# Fuzzy Search Bounds Formal Verification Traceability

Status: focused fuzzy-search slice for `fuzzy_symbol_search`, with the same
request-bound fix applied to `fuzzy_path_search` and `phonetic_symbol_search`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`fuzzy_symbol_search` at 24 calls. This slice covers pgmcp's local MCP boundary
before requests enter the verified fuzzy-search backends.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `fuzzy_symbol_search` | Clamp caller max edit distance and result limit before querying the persistent symbol trie; preserve per-project vocabulary isolation; return only hits within the effective distance. | `tla/FuzzySearchBounds.tla`; `pgmcp-testing/tests/tool_fuzzy_search_uses_persistent_trie.rs`; `src/fuzzy/limits.rs` unit tests. |
| `fuzzy_path_search` | Use the same bounded edit-distance/result-window policy for persistent path tries. | `tla/FuzzySearchBounds.tla`; shared `src/fuzzy/limits.rs`. |
| `phonetic_symbol_search` | Use the same bounded edit-distance/result-window policy before composed phonetic search over the persistent symbol vocabulary. | `tla/FuzzySearchBounds.tla`; shared `src/fuzzy/limits.rs`. |

## Issue Found And Corrected

The fuzzy tools accepted `u32` `max_distance` and `limit` parameters but cast
them directly to `usize`. Negative values are rejected by deserialization, but
oversized values such as `u32::MAX` could still request an impractically broad
edit-distance traversal or result window.

Correction: pgmcp now normalizes fuzzy request bounds in `src/fuzzy/limits.rs`:

| Bound | Default | Effective range |
| --- | --- | --- |
| `max_distance` | 2 | `0..=64` |
| `limit` | 20 | `1..=100` |

No sibling library code was changed for this slice.

## Inherited Contracts

This pgmcp slice relies on the formal-verification corpus in the fuzzy
dependencies rather than re-proving their internals:

| Dependency | Imported guarantee used here |
| --- | --- |
| `liblevenshtein-rust` | `docs/verification/tla/ValueYieldingQuery.tla` models value-yielding transducer query soundness, deduplication, skipping valueless finals, and termination; `PriorityQuery.tla`/Rocq distance proofs cover query-order and metric-adjacent surfaces within their stated boundaries. |
| `libdictenstein` | `formal-verification/README.md` records persistent trie/map, public read traversal, lock-free ARTrie publication, shared concurrency, and fuzzy candidate coverage contracts. These support pgmcp's use of `PersistentARTrieChar`-backed `FuzzyIndex` without adding hot-path locks in pgmcp. |
| `lling-llang` | `proofs/README.md` records WFST semiring, path/language, lazy composition, and ASR cascade-order proofs relevant to composed phonetic/WFST-style search reasoning. |
| `libgrammstein` | `formal/README.md` documents dependency bridge checks tying its query wrappers to liblevenshtein and libdictenstein contracts; pgmcp follows the same boundary style here. |

## Formal Model

`tla/FuzzySearchBounds.tla` models symbol, path, and phonetic-symbol requests
over project-local vocabularies with exact-match, ordinary, over-distance, zero
limit, and over-limit request values.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `EffectiveDistanceClamped` | Responses use the clamped caller max edit distance. |
| `EffectiveLimitClamped` | Responses use the clamped caller result limit. |
| `RowsProjectScoped` | Returned rows belong only to the requested project vocabulary. |
| `RowsWithinEffectiveDistance` | Returned rows are within the effective edit distance. |
| `OutputWithinLimit` | Returned rows never exceed the effective result window. |
| `ExactModeDoesNotAdmitTypos` | `max_distance=0` admits only exact-distance rows. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-FuzzySearchBounds \
  -config docs/formal/tla/FuzzySearchBounds.cfg \
  docs/formal/tla/FuzzySearchBounds.tla
```

Result: 4,084 distinct states, 4,084 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_fuzzy_search_uses_persistent_trie --build-jobs 1
```

Result: 3/3 passed. The new regression pre-populates a persistent symbol trie,
sends `max_distance = u32::MAX` and `limit = 0`, and verifies the response uses
`max_distance = 64` while returning exactly one hit.

```bash
cargo test -p pgmcp fuzzy::limits --lib
```

Result: 2/2 passed for the shared bound helper.
