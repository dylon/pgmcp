# Semver Break Audit Formal Verification Traceability

Status: focused API/fuzzy slice for `semver_break_audit`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `semver_break_audit` in the
2-call cluster. The tool compares current public symbols to a recent historical
surface parsed from git commit chunks, then uses the inherited
DAWG/Damerau-Levenshtein stack to identify likely renames.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `semver_break_audit` | Normalize project names; clamp commit windows to `1..=1000`; clamp output limits to `1..=250`; reject overlarge public-symbol snapshots rather than silently truncating the API surface; read the current `git_commit_chunks.content` schema column; scope commit chunks and symbols to the resolved project; stream rename-candidate selection without collecting all matches; execute read-only. | `tla/SemverBreakAuditBounds.tla`; `tool_sota_phase7_to_11` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool queried `git_commit_chunks.chunk_text`, but the current schema stores
the commit text in `git_commit_chunks.content`. This would fail against current
databases. The query now matches the schema used by `api_stability`.

`window_commits` and `limit` were used directly. `window_commits=0` produced an
empty historical scan, and negative `limit` values passed through Rust's cast to
`usize`. The tool now clamps the effective commit window and output limit before
querying or truncating.

The public API snapshot was unbounded, and rename selection collected all
candidate matches before choosing the best one. The tool now rejects projects
with more than 50,000 public symbols and streams `min_by` over the transducer
iterator, keeping only the current best rename candidate.

## Formal Model

`tla/SemverBreakAuditBounds.tla` models normalized project resolution, public
symbol cap checks, bounded commit-window scans, current-schema commit content
reads, bounded output, streaming rename selection, and read-only behavior.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectsDoNotScan` | Blank, missing, and duplicate projects reject before public-symbol and commit scans. |
| `PublicSymbolCapPreventsCommitScan` | Overlarge public API snapshots reject before commit scanning. |
| `EffectiveBoundsHold` | Commit windows and output rows are bounded by their effective clamps. |
| `UsesCurrentCommitChunkSchema` | Successful scans read `git_commit_chunks.content`, not the obsolete `chunk_text` column. |
| `StreamingBestSelection` | Rename selection keeps at most one candidate in memory. |
| `ReadOnly` | The tool performs no writes. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh SemverBreakAuditBounds.tla
```

Result: 8 distinct states, 15 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase7_to_11 --build-jobs 1
```

Result: pending until sibling `libgrammstein` builds successfully.

## Inherited Proof Surface

This tool reuses pgmcp's Damerau-Levenshtein + articulatory fuzzy stack
(the `rename_oracle` tool that shared it was removed 2026-06-13):

| Project | Inherited evidence used here |
| --- | --- |
| `liblevenshtein-rust/` | Damerau-Levenshtein transducer query correctness and transposition-distance behavior. |
| `libdictenstein/` | Dynamic DAWG construction/query invariants and dictionary deduplication. |
