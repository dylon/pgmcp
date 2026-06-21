# ADR-031 — Coupling/architecture metric resolution overhaul

**Status:** Accepted · **Date:** 2026-06-21 · Supersedes the import-resolution behavior implied by ADR-027 (hierarchical rollup).

## Context

An architecture review of the 16-crate `mettail-rust` workspace found every
module-level architecture metric **degenerate**: `coupling_cohesion_report`
reported `Ca=0, Ce=0, Instability=0, Abstractness=0, Distance=1.0,
zone=zone_of_pain` for all modules, and `architecture_quality` rolled that up to
`loose_coupling = F`. File-level fan-in was clearly populated (e.g.
`simulation/src/parikh.rs` in_degree=899), proving the coupling numbers were an
**analysis artifact, not real architecture**. The symptom reproduced on
single-crate **pgmcp itself** (76/78 modules degenerate, 239 import edges for
1,533 files), so it was not workspace-specific.

The metrics consume `import`-type edges in `code_graph_edges`. Those edges were
starved by several independent defects in `src/cron/graph_analysis.rs` and the
resolver `src/graph/import_extractor.rs`, compounded by a content-dependent
abstractness measure and a missing "this result is degenerate" signal. Two
adjacent defects surfaced in the same review (phantom index rows; a broken `\b`
regex in `grep`). The full scope was opted into by the user (no deferrals / no
technical debt), which additionally pulled in three formerly-deferred items
(cross-project resolution, persisted abstractness, crate-aware bucketing).

## Pipeline and the starvation points (before)

```
indexed_files.content ──(symbol-extraction; recovers content from disk)──▶ symbol_references(import_use)
        │  content=NULL for ~95% of files (intentional asymmetric storage,                  │ persist regardless of content
        │  src/db/disk_read.rs)                                                              │
        ▼                                                                                    ▼
graph-analysis · analyze_project
   Phase A: SELECT … WHERE content IS NOT NULL     ◀── D2 content gate: excludes the recoverable majority
   resolve_rust_import:
     use crate:: / super::  → resolved
     use <other_crate>::…   → Vec::new()           ◀── D1 resolver gap: cross-crate dropped (no ident→dir map)
        ▼
code_graph_edges(import) → compute_module_metrics(depth=2) → file_metrics
        │   abstractness from CONTENT (regex), hardcoded 0.0 in cron ◀── D3
        ▼
coupling_cohesion_report / architecture_quality   (no degeneracy guard ◀── D4)
```

### Decisive corrections found during design
- **C1** — the v48 `module_metrics`/`project_metrics` tables *already* have
  `abstractness`/`avg_abstractness`/`distance_from_main_sequence`/`avg_distance`
  columns, written by `hierarchy::rollup`, but always `0` because the cron passed
  `compute_module_metrics`'s hardcoded `abstractness=0.0`.
- **C2** — the cron persisted `distance_from_main_sequence = instability` (= `I`),
  not Martin's `|A+I−1|`. With `A≡0` the correct distance is `1−I`, so the
  persisted distance was **inverted** and `architecture_quality_score =
  1−avg_distance` (rollup.rs) was wrong on Rust. Calling `update_abstractness` in
  the cron is therefore a *correctness* fix, not only a feature.
- **C3** — the per-file graph subsystem is hard-scoped intra-project
  (`WHERE e.project_id=$1`); coupling/architecture/SDP/community readers also
  carry `tf.project_id = e.project_id`, so cross-project edges were *already*
  excluded from Martin metrics. The real risk of introducing cross-project edges
  was ~10 *unguarded* readers that would silently absorb them.

## Decision

Nine workstreams, no DB feature flags. Migrations: **v55** (file abstractness),
**v56** (cross-project edges).

| WS | Change | Key files |
|----|--------|-----------|
| **WS2** | Drop the `content IS NOT NULL` gate in `analyze_project`; the symbol-aware edge build is content-independent, and the regex fallback recovers content via `disk_read::read_disk_verified`. | `src/cron/graph_analysis.rs` |
| **WS1** | `CrateLayout` (`ident → src_dir`, from member `Cargo.toml`s); resolver external-crate + `self::` arms; widened `RUST_USE` regex. | `src/graph/cargo_layout.rs`, `src/graph/import_extractor.rs` |
| **WS9** | `ModuleBucketer` — Rust files bucket by Cargo **crate** (`crate:<ident>`), not fixed dir-depth; non-Rust falls back to depth-N. Cron always crate-aware; `coupling_cohesion_report` gains a `bucketing: depth\|crate` mode. | `src/graph/metrics.rs` |
| **WS8** | `v55` adds `file_metrics.is_abstract`/`abstract_type_count`/`concrete_type_count` (+ `hier_group_metrics.avg_abstractness`). Cron computes per-file abstractness content-independently from `file_symbols` (`trait`/`interface` vs type kinds) and calls `update_abstractness` before the rollup (fixes C1+C2). `architecture_quality` gains a `main_sequence_distance` dimension = `100·(1−avg_distance)`. | `src/db/migrations/v55_*.rs`, `graph_analysis.rs`, `tool_architecture_quality.rs` |
| **WS3** | Single source of truth: `coupling_cohesion_report` / `architecture_violations` / `fix_circular_dependency` read `file_metrics.is_abstract` via `queries::file_abstractions`; the content-regex and path-name heuristics (`is_abstract_file`, `ABSTRACT_PATTERNS`) are deleted. | `src/db/queries/symbols.rs`, three tools |
| **WS4** | `queries::coupling_degenerate(project)` (≥20 files AND **zero** coupled ⇒ degenerate). Surfaced in `orient.health`; `architecture_quality` marks `loose_coupling`/`sdp_compliance`/`code_organization`/`main_sequence_distance` **N/A** when degenerate. | `src/db/queries/topics.rs`, `tool_orient.rs`, `tool_architecture_quality.rs` |
| **WS7** | `v56` adds self-identifying `code_graph_edges.target_project_id`. `WorkspaceCrateMap` resolves cross-crate `use` into another indexed project (Tier-2), worktree-safely. ~10 intra-project readers filter `target_project_id IS NULL`; `dependency_graph` gains an opt-in `include_cross_project`. | `src/graph/workspace_crate_map.rs`, `graph_analysis.rs`, +readers |
| **WS5** | Phantom-row prune: a root-scoped, walk-success-gated set-difference sweep in `rescan_workspace` (mirrors the startup sweep); `cleanup_stale_files` hardened (batched, timeout-lifted, log-and-continue). | `src/indexer/event_processor.rs`, `src/db/queries/files.rs` |
| **WS6** | `grep` translates PCRE/GNU word-boundary escapes to PostgreSQL POSIX-ARE (`\b`→`\y`, `\B`→`\Y`, `\<`/`\>`→`\m`/`\M`), skipping bracket interiors. | `src/db/queries/search.rs` |

## Trust / correctness boundaries

- **Cross-project edges are self-identifying and opt-in.** `target_project_id`
  is `NULL` for every intra-project/unresolved edge (no backfill); set only for a
  resolved cross-crate edge into another project. The edge's `project_id` stays
  the source's project (the `DELETE … WHERE project_id=$1` rebuild lifecycle).
  Every intra-project per-file reader filters `target_project_id IS NULL`; only
  the unified-graph KG view and the deliberate `dependency_graph` opt-in include
  them. Martin metrics are unaffected (C3).
- **Worktree safety.** `WorkspaceCrateMap::pick_entry` resolves cross-crate `use`
  via, in priority order: the same project → a precise cargo `path=` dependency
  (`project_dependencies`) → the worktree-group **main** only (never a clone) →
  a single foreign group; it fails closed on ambiguity. This prevents fabricated
  edges between a project and its git-worktree clones. Worktree families come
  from the existing pure `hierarchy::grouping::derive_groups`.
- **Phantom prune is conservative.** Root-scoped to the walked workspace,
  walk-success-gated (skipped on walk failure or an empty walk), and
  `Missing`-only (an edited file still `exists()` and is re-indexed, never
  pruned). FK cascades from `indexed_files` clear all child rows.
- **Degeneracy floor.** `coupling_degenerate` requires ≥20 files with metrics AND
  *exactly zero* coupled files — false-positive-proof (no real 20+-file codebase
  has zero internal imports) and keeps small fixtures/leaf crates scored.

## Consequences

- Coupling/architecture metrics compute from real, resolved edges on single-crate
  and workspace projects; abstractness and main-sequence distance are
  content-independent and persisted at all four hierarchy tiers.
- A one-time metric **step-change** in `project_metrics.avg_distance` /
  `architecture_quality_score` (and `quality_trend` history) occurs on the first
  post-fix cron run, because the old persisted distance was inverted (C2).
- Cross-project file→file import edges now exist; consumers must opt in.
- Activation requires a daemon rebuild + restart (so the new cron/tool/migration
  code is live) followed by `symbol-extraction` then `graph-analysis`.

## Verification

- Unit: `cargo_layout` (ident≠dir, `crate_of_path`), resolver external/`self::`
  arms, `ModuleBucketer` (crate keys, shim parity, cross-crate inter-module),
  `workspace_crate_map::pick_entry` (worktree safety), `translate_pcre_boundaries`,
  `file_abstract_type_counts` kind-string golden.
- Real-DB oracle: `architecture_quality` 11 dimensions + degeneracy-guard N/A +
  `main_sequence_distance` scoring; `coupling_cohesion_report` content-independent
  abstractness; `dependency_graph` cross-project opt-in.
- End-to-end (post-restart): `coupling_cohesion_report(pgmcp, depth=2)` reports
  non-zero `Ca+Ce` for most `src/*`; on a `mettail-rust` project the `ast` /
  `prattail` / `runtime` hubs report non-zero coupling and `orient.health.
  coupling_degenerate` flips `true→false`; `grep '\bword\b'` matches as `rg` does.
