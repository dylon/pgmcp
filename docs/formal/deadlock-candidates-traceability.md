# Deadlock Candidates Formal Verification Traceability

Status: focused concurrency/read slice for the legacy `deadlock_candidates` tool.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `deadlock_candidates` at 2
calls. The tool scans indexed Rust content in one project, builds adjacent
lock-order edges, reports SCC cycles, and enriches the result with mutex-typed
symbols and effect counts.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `deadlock_candidates` | Resolve the requested project fail-closed; scan file content only from that project; construct lock-order edges and SCC cycles only from scoped rows; keep mutex-typed symbol enrichment project-scoped; keep effect-count enrichment project-scoped; remain read-only with no persistent locks. | `tla/DeadlockCandidatesScope.tla`; `oracle_deadlock_candidates`; filtered `tool_sota_phase5`. |

## Issues Found And Corrected

The primary file scan and mutex-typed symbol query used the resolved
`project_id`, but `effect_breakdown` counted all `symbol_effects` rows in the
workspace. That could leak unrelated project effect metadata into a scoped
deadlock report.

Correction: `effect_breakdown` now uses the shared `effect_counts(pool,
project_id)` helper, joining `symbol_effects` through `file_symbols` and
`indexed_files` to the same resolved project id as the primary scan.

## Formal Model

`tla/DeadlockCandidatesScope.tla` models project resolution, scoped file scans,
scoped lock-order edges and cycles, scoped mutex-typed symbols, scoped effect
counts, and read-only execution.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidProjectNoScan` | Missing projects reject before scan, edges, or cycles. |
| `FileScanProjectScoped` | Content scanning reads only the resolved project. |
| `EdgesProjectScoped` | Lock-order edges cannot include another project's rows. |
| `CyclesFromScopedEdgesOnly` | Cycles are reported only from scoped lock-order edges. |
| `EffectBreakdownProjectScoped` | Effect counts do not include another project's symbol effects. |
| `MutexTypedSymbolsProjectScoped` | Mutex-typed symbol enrichment stays in the resolved project. |
| `ReadOnlyNoLock` | The tool has no write or held-lock path. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_deadlock_candidates --build-jobs 1
```

Result: 1/1 passed for project-scoped edges, cycles, and effect breakdown.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase5 \
  deadlock_candidates_runs --build-jobs 1
```

Result: 1/1 passed for the existing `deadlock_candidates` smoke path.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh DeadlockCandidatesScope.tla
```

Result: TLC exit 0; 2 distinct states, 4 states generated; no invariant
violations.
