# Reindex Serialization Formal Verification Traceability

Status: focused operational slice for the destructive `reindex` tool.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `reindex` at 3 calls. The
tool is intentionally destructive: full mode clears chunks in bounded batches
before clearing files, while language mode clears only files and chunks for one
normalized language so the background scanner can re-extract that subset.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `reindex` | Reject invalid language tokens before locking or writing; normalize valid language tokens; serialize destructive runs through a non-blocking lock; reject concurrent runs without writes; delete only matching language rows in language mode; delete chunks before files in full mode; surface cancellation before file deletion; release the lock on every response path. | `tla/ReindexSerializationScope.tla`; `oracle_reindex`; `reindex_serialization`. |

## Issues Found And Corrected

`reindex` accepted the optional `language` string verbatim. Blank values,
case-mismatched values, path-like tokens, and very long values could reach the
database helper. Most of those inputs were harmless no-ops, but the boundary was
ambiguous for an operational tool that deletes rows.

Correction: the language parameter now trims whitespace, lowercases ASCII, caps
the token at 64 bytes, and rejects blank or non-token characters before acquiring
the reindex lock.

## Formal Model

`tla/ReindexSerializationScope.tla` models language validation, normalization,
non-blocking lock acquisition, busy-lock rejection, daemon-stopping cancellation
points, language-scoped deletion, full-reindex chunk-before-file ordering, bounded
chunk batches, and lock release.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidLanguageNoWrite` | Invalid language tokens reject before lock acquisition or writes. |
| `BusyLockNoWrite` | A busy reindex lock rejects without writes. |
| `NoConcurrentReindex` | A request can acquire the destructive lock only when it is free. |
| `DaemonStoppingBeforeNoWrite` | A stopping daemon cancels after lock acquisition but before destructive writes. |
| `LanguageNormalized` | Accepted trimmed language input normalizes to the stored token. |
| `LanguageModeScoped` | Language mode deletes the target language and preserves other languages. |
| `FullDeleteChunksBeforeFiles` | Full mode deletes file rows only after chunk deletion completes. |
| `CancellationStopsBeforeFileDelete` | Mid-run cancellation prevents full-mode file deletion. |
| `BatchedDeleteBounded` | Full-mode chunk-delete batches stay within the configured batch cap. |
| `LockReleased` | No response path leaves the reindex lock held. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_reindex --build-jobs 1
```

Result: 2/2 passed for language normalization and invalid-token fail-closed
coverage.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh ReindexSerializationScope.tla
```

Result: TLC exit 0; 9 distinct states, 18 states generated; no invariant
violations.
