# Complexity Hotspots Formal Verification Traceability

Status: focused high-use-tool slice for `complexity_hotspots`, the next
telemetry-ranked tool after `experiment_log_artifact`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this slice showed
`complexity_hotspots` at 34 calls. The tool ranks files by structural and AST
complexity signals inside one project.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `complexity_hotspots` | Clamp caller limits to a finite result cap; fail closed on duplicate project display names; resolve unique project names to project ids before the real DB ranking query; return only rows belonging to the resolved project id. | `tla/ComplexityHotspotsScoping.tla`; `pgmcp-testing/tests/oracle_complexity_hotspots.rs`. |

## Issues Found And Corrected

`complexity_hotspots` previously used `params.limit.unwrap_or(20)` and then
cast the signed value to `usize` for `truncate`. A negative limit therefore
became a huge unsigned value and effectively disabled the cap.

Correction: limits are now clamped to `1..=100`, matching the bounded-limit
pattern used by nearby MCP tools.

The tool also queried complexity and AST/effect enrichment by project display
name. Duplicate display names could therefore merge rows from multiple indexed
projects or pick an arbitrary project id for enrichment.

Correction: the tool rejects ambiguous display names up front. For the real DB
path, a unique match is queried by `project_id`, avoiding a name-check/name-query
race.

## Formal Model

`tla/ComplexityHotspotsScoping.tla` models unique, duplicate, and missing project
names with negative, zero, normal, and oversized limits.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `AmbiguousProjectRejected` | Duplicate project display names never produce ranked rows. |
| `AcceptedRowsProjectScoped` | Accepted rows belong only to the resolved project id. |
| `EffectiveLimitClamped` | Each response uses exactly the clamped caller limit. |
| `OutputWithinLimit` | The returned row set never exceeds the effective limit. |
| `MissingProjectReturnsNoRows` | Missing projects do not leak rows from other projects. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_MEMORY_MAX=768M PGMCP_TLC_JAVA_XMX=512m \
  PGMCP_TLC_WORKERS=1 timeout 60 scripts/tlc-capped.sh \
  -config docs/formal/tla/ComplexityHotspotsScoping.cfg \
  docs/formal/tla/ComplexityHotspotsScoping.tla
```

Result: 25 distinct states, 50 generated states, no invariant violations. The
model was refactored from call-sequence exploration to a one-shot arbitrary
request, preserving the per-call safety invariants while avoiding unnecessary
state-history growth.

```bash
cargo nextest run -p pgmcp-testing --test oracle_complexity_hotspots --build-jobs 1
```

Result: 6/6 passed. The regression tests cover the hand-computed composite
formula, sort-by-size behavior, coupling contribution, negative-limit clamping,
oversized-limit capping, and duplicate project display-name rejection.
