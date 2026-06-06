# API Stability Formal Verification Traceability

Status: focused API-contract slice for `api_stability`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `api_stability` at 3 calls,
the first uncovered tool after the 4-call group. The tool is read-only: it
loads recent git commit chunks for a resolved project, detects public signature
changes, and reports low-stability symbols.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `api_stability` | Reject blank/missing/duplicate projects through the shared resolver; clamp the commit window and output limit; read the migrated `git_commit_chunks.content` column; return only symbols from commits in the resolved project; reuse the resolved project id for effect enrichment; remain read-only with no persistent locks. | `tla/ApiStabilityScope.tla`; filtered `tool_sota_phase7_to_11` tests. |

## Issues Found And Corrected

The tool selected `git_commit_chunks.chunk_text`, but the migrated schema stores
the indexed commit payload in `git_commit_chunks.content`. The existing smoke
test did not seed commit chunks, so this schema drift was not exercised.

Correction: the query now reads `gcc.content`, and the focused regression test
seeds real git commit/chunk rows.

The tool accepted raw `window_commits` and `limit` values. Negative limits could
be cast into huge `usize` truncation values, while a zero/huge window either
misrepresented the requested scan or risked an excessive read.

Correction: `window_commits` is clamped to `1..=1000`; output `limit` is clamped
to `1..=250`.

Effect enrichment performed a second project lookup by the raw project string.
That could diverge from the already-resolved project id after trimming or if
the name became ambiguous.

Correction: enrichment now uses the same resolved `project_id` as the commit
query.

## Formal Model

`tla/ApiStabilityScope.tla` models unique, blank, missing, and duplicate
project resolution; low/high numeric bounds; current-schema commit chunk reads;
same-project signature rows; and same-project effect enrichment.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectsReject` | Invalid project resolution returns no rows. |
| `WindowAndLimitBounded` | Commit windows and output rows stay within finite bounds. |
| `UsesCurrentCommitChunkColumn` | Accepted requests read `content`, not stale `chunk_text`. |
| `CommitRowsStayProjectScoped` | Reported signature rows belong to the resolved project. |
| `EffectEnrichmentUsesResolvedProject` | Effect enrichment uses the same resolved project id. |
| `NoCrossProjectLeak` | A signature from another project cannot appear in output. |
| `ReadOnlyNoHeldLock` | The model has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase7_to_11 api_stability --build-jobs 1
```

Result: 3/3 passed for the filtered API-stability slice.

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh ApiStabilityScope.tla
```

Result: exit 0; no invariant violations; 5 distinct states and 10 states
generated at depth 1.
