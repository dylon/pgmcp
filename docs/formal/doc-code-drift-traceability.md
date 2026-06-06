# Doc Code Drift Formal Verification Traceability

Status: focused doc/code embedding-drift slice for `doc_code_drift`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `doc_code_drift` at 2
calls. The tool resolves a project, computes per-directory cosine distance
between markdown and code chunk centroids, filters by a minimum drift threshold,
limits output rows, and enriches the response with Shadow-ASR effect counts.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `doc_code_drift` | Reject missing/duplicate/blank projects; trim project input; normalize finite drift thresholds and row limits; apply the row bound in SQL before materializing output; keep drift rows and effect counts scoped to the resolved project id; remain read-only with no persistent locks. | `tla/DocCodeDriftScope.tla`; `oracle_doc_code_drift`; filtered `tool_sota_phase4`. |

## Issues Found And Corrected

The primary drift query already used `project_id_or_err`, which fails closed for
missing or duplicate project display names. The response and effect enrichment
still used `params.project` directly, so a whitespace-padded project name could
successfully resolve for the drift query while returning the unnormalized project
string and skipping effect enrichment.

Correction: the tool now trims `project` once, uses that value for project
resolution and response metadata, and calls `effect_counts(pool, project_id)` on
the already-resolved id.

The old code fetched every directory row, filtered in Rust, then truncated the
vector. A large project with many directory pairs could allocate more output
than the caller's requested limit justified.

Correction: `min_drift` is normalized to the cosine-distance domain `0..=2`,
`limit` is clamped to `0..=100`, and both are pushed into the SQL query with
`WHERE dist >= $2` and `LIMIT $3`.

## Formal Model

`tla/DocCodeDriftScope.tla` models fail-closed project validation, trimmed valid
input, bounded threshold/limit normalization, SQL-bounded returned rows,
project-scoped drift/effect channels, and read-only execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectRejects` | Missing, duplicate, and blank projects reject before scans. |
| `TrimmedProjectAccepted` | Whitespace-normalized valid project input reaches the ok path. |
| `ThresholdBounded` | Effective drift threshold stays in `0..=2` (modeled as `0..200`). |
| `LimitBounded` | Effective row limit stays in `0..=100`. |
| `ReturnedRowsSqlBounded` | Materialized rows never exceed the effective SQL limit. |
| `DriftRowsProjectScoped` | Directory drift rows are read only from the resolved project. |
| `EffectBreakdownResolvedProjectScoped` | Effect enrichment cannot include another project's effects. |
| `ReadOnlyNoLock` | The tool has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_doc_code_drift --build-jobs 1
```

Result: 3/3 passed for trim/scoped enrichment/read-only behavior, duplicate
project rejection, and threshold/limit clamping.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase4 \
  doc_code_drift_runs --build-jobs 1
```

Result: 1/1 passed for the existing SOTA Phase 4 smoke path.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh DocCodeDriftScope.tla
```

Result: TLC exit 0; 6 distinct states, 12 states generated; no invariant
violations.
