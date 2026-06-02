# ADR-013: Symbol extraction reads content-NULL files from disk

**Status:** Accepted (2026-06-02)
**Related:** ADR-003 (tag-set vocabulary), the asymmetric-storage policy in
`src/embed/pool.rs`, scientific-ledger `symbol-coverage-rc1-rc2-2026-06-02.md`.

## Context

pgmcp stores file bytes asymmetrically: for plain-text files whose source is
cheap to re-read from disk, the indexer (`src/embed/pool.rs`) sets
`indexed_files.content = NULL`, `content_recoverable_from_disk = true`, and keeps
only `content_hash` (xxHash3-64). This keeps the DB small; consumers that need
the full text re-read it from disk and verify it against `content_hash` (the
`read_file` MCP tool already did this).

The symbol-extraction cron did **not**. `list_files_for_symbol_extraction`
filtered `AND content IS NOT NULL` and Phase B did `None => continue`. So every
disk-backed file was invisible to extraction. For a continuously re-indexed
project (e.g. pgmcp itself), ~91% of source files are content-NULL at any moment,
so symbol coverage collapsed to the inline-content minority and the project could
not fuzzy-search its own code. (A project indexed once under an old full-content
snapshot and never re-scanned, like `f1r3node`, retained inline content and was
unaffected — which is why coverage looked fine elsewhere.)

## Decision

Symbol extraction recovers content-NULL files from disk, hash-verified, via a
**shared** helper rather than abandoning the storage policy:

- New `src/db/disk_read.rs`: `read_disk_verified(path, recoverable,
  expected_hash) -> DiskReadOutcome` (`Hit | HashMismatch | Missing | IoError |
  NotRecoverable`) and `content_hash_i64(bytes)`. The `read_file` tool was
  refactored onto the same helper so the two disk fast-paths cannot drift.
- `list_files_for_symbol_extraction` / `fetch_file_content_batch` drop the
  `content IS NOT NULL` filter and additionally select `path`,
  `content_recoverable_from_disk`, `content_hash`, `extracted_content_hash`,
  `modified_at`.
- Phase B: inline `content` when present, else `read_disk_verified`. A
  `HashMismatch` (file edited since indexing) / `Missing` / `IoError` /
  `NotRecoverable` is counted (`symbol_extraction_disk_*`) and skipped — and
  holds the watermark (F1) so the next run retries it after the indexer refreshes
  the hash.
- An unchanged file is skipped without re-parsing via a new nullable
  `indexed_files.extracted_content_hash` (migration v24) compared to the current
  `content_hash` — required so that, with the `content IS NOT NULL` gate removed,
  a full re-scan does not re-read+re-parse every file from disk each time.

## Consequences

- Extraction is now **correct regardless of the content-null race** — symbols no
  longer depend on catching a file during its (non-existent, for Rust) inline
  window. The disk read happens outside any DB transaction, so it is not bound by
  the per-file 15s `statement_timeout`; one disk string is held at a time
  (per-file, dropped before the next).
- Extraction is now **coupled to filesystem availability and `content_hash`
  integrity**. A file deleted/edited between index and extraction is skipped
  (counted, retried later) rather than mis-parsed — a deliberate
  correctness-over-coverage choice on stale input.
- New observability: `symbol_extraction_disk_reads / _disk_hash_mismatches /
  _disk_missing / _disk_io_errors / _unchanged_skips`.

## Alternatives considered

- **Stop nulling backend-language files in `embed/pool.rs`** (keep their inline
  content). Rejected: it re-stores content for exactly the largest files,
  re-growing the DB and defeating the storage optimization, and leaves the cron
  permanently fragile to any future NULL.
- **Auto-insert/auto-tolerate in the hot path** (e.g. upsert unknown effects, or
  parse whatever is in chunks). Out of scope here; the chunk-stitch fallback
  remains only in `read_file`, which is a human-facing read, not an
  authoritative parse.
