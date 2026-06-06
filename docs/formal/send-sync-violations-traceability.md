# Send Sync Violations Formal Verification Traceability

Status: focused concurrency-safety slice for `send_sync_violations`.

## Scope

`send_sync_violations` was the next unverified telemetry-ranked concurrency
tool. The slice covers request validation, project scoping, memory-bounded file
scanning, and read-only behavior.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `send_sync_violations` | Trim and reject blank project names; reject unknown/duplicate projects via strict project-id resolution; clamp `limit` to 1..200; stream indexed Rust files instead of loading all file contents up front; stop scanning once the normalized match limit is reached; return only Rust-file regex matches from the resolved project; scope unsafe-effect symbol enrichment to the same project id; remain read-only and lock-free. | `tla/SendSyncViolationsScan.tla`; `pgmcp-testing/tests/tool_sota_phase5.rs`. |

## Issues Found And Corrected

The tool passed `limit.max(0) as usize` to the scanner. Negative limits produced
zero results, and oversized limits were unbounded. Correction: the tool clamps
`limit` to 1..200 and echoes the normalized value.

The shared regex scanner fetched all matching file contents with `fetch_all`
before applying the match limit in memory. Large projects could therefore use
far more RSS than the requested result bound implied. Correction: the scanner
now streams rows in stable `indexed_files.id` order and returns as soon as the
match limit is reached. It also handles `limit == 0` as an immediate empty
result for other callers that intentionally pass zero.

The tool response echoed the raw project string. Correction: it now trims the
project before strict resolution and response construction.

## Concurrency Boundary

`send_sync_violations` is read-only. It performs SELECTs only, opens no
advisory locks, no row locks, and no process mutexes. Its concurrency safety is
scoping and boundedness: all scans and unsafe-symbol enrichment use the same
resolved immutable `project_id`, and the scanner does not keep a growing file
corpus resident while other tasks run.

## Formal Model

`tla/SendSyncViolationsScan.tla` models representative valid, blank, duplicate,
unknown, negative-limit, and oversized-limit requests over a small indexed-file
universe.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Blank, unknown, and duplicate projects reject. |
| `SuccessfulRequestsResolvedUniqueProject` | Successful scans use exactly one resolved project id. |
| `LimitBound` | Result count never exceeds the normalized 1..200 limit. |
| `RustFilesOnlyAndProjectScoped` | Scanned/matched files are Rust files in the resolved project. |
| `StreamingStopsAtLimit` | The scan does not continue reading rows once the limit is satisfied. |
| `UnsafeSymbolsUseSameProject` | Unsafe-effect enrichment uses the same resolved project id. |
| `ReadOnlyNoLocks` | The tool writes nothing and holds no locks. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh SendSyncViolationsScan.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 6 distinct states, 12
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 -- send_sync --build-jobs 1
```

Result: 3/3 passed, 10 unrelated tests skipped by the nextest filter. The
focused run covers ordinary execution, planted `static mut`/`unsafe impl
Send`/`Arc<RefCell>` matches, limit clamping, trimmed project echo, and
duplicate project display-name rejection.
