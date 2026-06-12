# Work Summary Formal Verification Traceability

Status: request-boundary slice for the `work_summary` MCP tool.

## Scope

`work_summary` summarizes a time period's work (typically a month) across the git
repos in a workspace. It reads commit facts (including line churn) live from
`git log --numstat`, reads uncommitted/mid-stream state from the working tree, and
consults the temporal-graph index only as a freshness-gated enrichment. Because
workspace enumeration is itself the first side effect, the verification slice
focuses on the request boundary: every invalid parameter must reject *before* any
repo scan / DB query / render; the repo scan and per-repo reads must be bounded;
topic enrichment must be gated on freshness; and the rendered envelope must echo a
canonical format.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_summary` | Resolve workspace_root (explicit → first `[workspace]` path) and reject when none; parse `month` / `since`/`until` and reject malformed windows and `since >= until`; reject an explicitly-blank `author`; restrict `format` to `markdown\|org\|json`; reject unknown `group_by` / `use_graph`; clamp `max_repos` and `limit` to `1..=1000`; scan at most `min(available_repos, clamp(max_repos))` canonical repos; perform one `git log --numstat` pass + one reconciliation query per scanned repo (no unbounded per-repo loop); attach indexed topics only when `use_graph != off` AND the project index is fresh; render exactly one envelope whose `normalized` block echoes the resolved parameters. | `tla/WorkSummaryBoundary.tla`; `src/worklog/mod.rs::WorkSummaryRequest::from_params` + `summarize`; `worklog` unit tests; `oracle_work_summary` once the populated index is available. |

## Issues Found And Corrected

The boundary was built to the obligations above; the design decisions that enforce
them:

- **Validation precedes work.** `from_params` returns `McpError::invalid_params`
  for every malformed input and only then constructs a `WorkSummaryRequest`;
  `summarize` consumes that already-validated value, so no repo is scanned and no
  query is issued on a rejected call. (Unlike `quality_report`, there is no
  project-lookup phase to leak — workspace enumeration *is* the first side effect,
  so all rejects are strictly local.)
- **Format restricted at the boundary.** `ReportFormat::parse` admits six
  renditions, but `work_summary` only implements three; `restricted_format` rejects
  `latex`/`html`/`text` with a canonical message instead of rendering an empty
  document, and the accepted format is echoed back in `normalized.format`.
- **Bounded scan.** `max_repos`/`limit` are clamped to `1..=1000`; the canonical
  repo set (worktree-deduped via `pick_main_worktree_ids`) is truncated to
  `max_repos`, and the per-repo work is a single git pass plus a single
  reconciliation query — there is no nested loop over a repo's history.
- **Freshness gate.** Topic enrichment runs only when `use_graph != off` and the
  per-project freshness verdict is `fresh` (live HEAD == indexed `git_last_commit`);
  otherwise the report records `stale`/`unindexed` and proceeds on the
  authoritative live-git numbers alone.

## Formal Model

`tla/WorkSummaryBoundary.tla` models the wrapper as a phased boundary: local
validation/normalization/clamping → bounded canonical-repo scan → bounded per-repo
git + DB reads → freshness-gated enrichment → one rendered, canonical-format
envelope.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `TypeOK` | The response record stays within its declared domains. |
| `LocalRejectsHaveNoSideEffects` | Any reject (no workspace, bad window, blank author, bad format/group/graph) performs zero repo scans, git passes, DB queries, and renders. |
| `ResolveBeforeWork` | A positive `repos_scanned` implies the request was accepted. |
| `BoundedRepoScan` | `repos_scanned <= min(available, clamp(max_repos))`, `clamp(max_repos) ∈ 1..1000`, and rejects scan nothing. |
| `ChurnReadsBounded` | Git passes and DB queries are each `<= repos_scanned` (one pass each, no unbounded per-repo loop). |
| `EnrichmentGatedByFreshness` | Topic reads occur only when accepted and `use_graph != off`; topics are attached only when accepted, graph-enabled, AND fresh. |
| `OneRenderPerAccept` | Accepted calls render exactly one envelope; rejected calls render none. |
| `CanonicalEnvelopeFormat` | Successful envelopes expose a canonical `markdown\|org\|json` format, never an input alias. |

## Verification Run 2026-06-11

```bash
cd docs/formal/tla
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkSummaryBoundary.tla
```

Result: 11 distinct states, 21 generated states, no invariant violations.

```bash
cargo nextest run --bin pgmcp worklog
```

Result: 8 `worklog` unit tests pass (conventional-commit parsing, numstat churn
summation, shortstat parsing, month/instant parsing, top-n ordering, path-under and
dir-normalization). The real-data `oracle_work_summary` integration test
(`pgmcp-testing/tests/`) asserts the reproduced May-2026 figures against a populated
index and is `#[ignore]`-gated on that index being present.
