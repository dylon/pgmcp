# Experiment Render Ledger Formal Verification Traceability

Status: focused render/write slice for `experiment_render_ledger`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows
`experiment_render_ledger` at 2 calls. The tool fetches an experiment by id or
slug, renders a markdown ledger, and either returns it without writing
(`dry_run=true`) or writes a ledger file under the configured experiment ledger
directory.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_render_ledger` | Validate lookup inputs; trim slug lookup; reject unsafe ledger directories and stored slugs; ensure dry-run returns content without writing; ensure non-dry writes stay inside the configured relative ledger directory; publish files atomically; avoid DB mutation and persistent locks. | `tla/ExperimentRenderLedgerScope.tla`; `oracle_experiment_render_ledger`; filtered `tool_experiments_integration`. |

## Issues Found And Corrected

The render helper joined the configured `ledger_dir` directly to the current
directory and joined `core.slug` directly into the filename. A malformed config
such as `../outside`, or a corrupted/directly-seeded experiment slug containing
path separators, could escape the intended ledger directory.

Correction: `render_and_write` now rejects empty, absolute, root, prefix, and
parent-containing ledger dirs. It also rejects stored slugs that are empty, too
long, or contain anything other than ASCII letters, digits, `-`, or `_`.

The old write path used `std::fs::write` directly on the final ledger path. A
concurrent reader could observe a partially-written file, and concurrent
renders could truncate the visible file before content was fully written.

Correction: non-dry renders now write to a UUID-suffixed temporary file in the
target directory and then rename it into place. The rename is the modeled atomic
publish point; successful renders leave no temp file behind.

Slug lookup was passed through literally.

Correction: slug lookup is trimmed, and blank slug with no experiment id now
rejects before querying.

## Formal Model

`tla/ExperimentRenderLedgerScope.tla` models valid and invalid lookup inputs,
safe and unsafe ledger directories, safe and unsafe stored slugs, dry-run vs
write mode, atomic publish, and the absence of DB mutation or held locks.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidInputsReject` | Invalid lookup, directory, or stored slug inputs reject without content or file writes. |
| `TrimmedSlugLookupAccepted` | Whitespace-padded valid slug lookup reaches the ok path. |
| `DryRunWritesNothing` | Dry-run requests never write a ledger file. |
| `DryRunReturnsContent` | Accepted dry-run requests return rendered markdown content. |
| `WriteModePublishesOneContainedFile` | Accepted write-mode requests write a file under the safe ledger dir with a safe filename. |
| `AtomicNoPartialFile` | The model has an atomic publish point and no visible partial file state. |
| `NoDbMutationNoLock` | Rendering has no DB write or held-lock path. |

## Verification Run 2026-06-06

Pending Rust execution: `cargo nextest run -p pgmcp-testing --test
oracle_experiment_render_ledger --build-jobs 1` is currently blocked by an
unrelated compile error in sibling dependency `libdictenstein`:
`DurableOverlayWrite` impls are missing `value_present_faulting`,
`value_read_faulting`, and `value_publish_inner`.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh ExperimentRenderLedgerScope.tla
```

Result: TLC exit 0; 10 distinct states, 20 states generated; no invariant
violations.
