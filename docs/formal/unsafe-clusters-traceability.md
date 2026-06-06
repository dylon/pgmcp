# Unsafe Clusters Formal Verification Traceability

Status: focused high-use quality slice for `unsafe_clusters`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this sequence showed
`unsafe_clusters` at 13 calls. The tool combines a Rust-only regex scan with
typed effect-symbol enrichment; the quality aggregate collector also reads
typed `function_metrics`.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `unsafe_clusters` | Trim and uniquely resolve project names; reject blank/duplicate projects; clamp file-result limits; scan only Rust files in the resolved project; rank by unsafe-block count with deterministic path ties; scope effect-symbol enrichment to the same project id; keep the typed quality collector aligned across function metrics, symbols, and files. | `tla/UnsafeClustersScope.tla`; `pgmcp-testing/tests/tool_sota_phase5.rs`. |

## Issues Found And Corrected

The shared SOTA `project_id_or_err` helper used `fetch_optional` by project
name. Duplicate display names could resolve to an arbitrary project.

Correction: the helper now trims project names, rejects blank names, and fails
closed when a name matches multiple projects.

`unsafe_clusters` used raw project text in its response and converted negative
limits to zero rows. Correction: output project names are normalized and limits
are clamped to `1..=200`.

Per-file ranking sorted only by unsafe-block count, leaving ties unstable.
Correction: ties now sort by relative path.

The typed quality collector joined `function_metrics`, `file_symbols`, and
`indexed_files` without asserting that the symbol/file project identity matched
the metric project. Correction: collector rows now require
`f.project_id = fm.project_id` and `fs.file_id = f.id`.

## Formal Model

`tla/UnsafeClustersScope.tla` models blank, duplicate, missing, and trimmed
project names; low/high limits; Rust and non-Rust files; a cross-project
metric/file drift row; unsafe effect symbols; and typed function metric rows.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectsRejected` | Blank, duplicate, and missing projects return no data. |
| `ProjectNormalized` | Accepted responses report the trimmed project name. |
| `EffectiveLimitClamped` / `OutputWithinLimit` | File rows never exceed the clamped limit. |
| `RegexRowsScopedRustOnly` | Regex-derived rows are Rust files in the resolved project. |
| `RankingSound` | Returned rows are not outranked by omitted visible rows. |
| `TotalCountUsesScopedRustRows` | Total unsafe count is computed from scoped Rust hits. |
| `EffectSymbolsProjectScoped` | Effect enrichment cannot leak cross-project symbols. |
| `CollectorRowsProjectAndFileScoped` | Typed quality findings require metric/file/symbol identity agreement. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh UnsafeClustersScope.tla)
```

Result: 5 distinct states, 10 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 unsafe_clusters --build-jobs 1
```

Result: 2/2 passed. The focused run covers normalized project output,
limit clamping, per-file unsafe count ranking, total unsafe count, and
duplicate project display-name rejection.
