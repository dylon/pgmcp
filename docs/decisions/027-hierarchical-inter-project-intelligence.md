# ADR-027: Hierarchical inter+intra-project intelligence

- **Status:** Accepted — **delivered**: grouping (E1), multi-ecosystem deps (E2, v47),
  hierarchical rollup (E3, v48) + `workspace_architecture_quality`, inter-project coupling
  (E4) `cross_project_coupling`, CVE propagation (E6) `cross_project_cve_exposure`, and the
  category layer (item 4, ADR-028). The `cross_project_symbol_edges` opt-in overlay (v49) and
  the optional categorical cache (v51) remain the one genuinely-optional overlay (off by
  design); everything else below is built. (The "roadmap" tables below are the as-built map.)
- **Date:** 2026-06-19
- **Relates to:** item 15 (inter-project architecture) + item 4 (category theory). Module:
  `src/hierarchy/`. Migration: v46 (`project_groups`, `project_group_members`); v47–v51
  reserved (below).

## Context

pgmcp's structural analysis is **intra-project only**: `code_graph_edges`,
`symbol_references`, `file_metrics`, `function_metrics` are built per project, and ~30
architecture/design/security tools take a single required `project`. What already crosses
projects is narrow (`project_dependencies` Cargo-only single-hop; `cross_project_similarities`;
`cross_language_signature_clones`; global topics; `memory_unified_*`). Missing: a
project-grouping model, a persisted module/project/group/workspace **metric rollup**,
project-level cycle detection, and inter-project coupling metrics — and a way to **combine**
inter- and intra-project signal into one architectural-intelligence surface.

The unifying structure is the **containment chain**
`symbol ⊳ function ⊳ file ⊳ module ⊳ project ⊳ group ⊳ workspace`. It is simultaneously: the
object-chain of a **Containment functor** (item 4), and the level ladder over which metrics
**roll up** (item 15). Phase E *produces* the leveled data; the category subsystem *checks*
the functor laws over it.

## Decision — staged

### Stage 1 (landed): grouping model + level vocabulary — v46

`src/hierarchy/` defines the closed ADR-003 vocabularies:
`GroupKind{worktree_family, monorepo, declared, manual}`, `GroupRole{main, member}`, and the
level ladder `HierLevel{symbol, function, file, module, project, group, workspace}` (the
discriminator carried by every rollup metric row).

Two **link tables** (v46) — not a column on `projects`, because memberships overlap, groups
carry their own metric grain, and the mapping is re-derivable:

- `project_groups(id, kind, group_key, label, created_at, UNIQUE(kind, group_key))`
- `project_group_members(group_id, project_id, role, valid_from, valid_to)` with a partial
  unique index on the open interval (`valid_to IS NULL`, the v28 bitemporal idiom) so
  re-grouping is non-destructive.

Worktree families generalize the existing `pick_main_worktree_ids` (same git-common-dir +
root-commits ⇒ one group, shortest-basename = `main`); singletons become singleton groups;
monorepos come from multi-manifest-under-one-root detection; declared groups from
`.pgmcp.toml [group]`.

### Stage 2–6 (roadmap): the rest of item 15

| vNN | Contents |
|----|----------|
| v47 | widen `project_dependencies.source` CHECK for new `DepSource` arms (`npm`, `pypi`, `go`, `maven`, `lake`) — multi-ecosystem manifest parsing (`src/deps/{npm,pypi,go,maven,lake}_manifest.rs`) + closure / project SCC-cycles / DSM (`src/hierarchy/dep_graph.rs`) |
| v48 | `module_metrics`, `project_metrics`, `hier_group_metrics` (each `level`-discriminated via `HierLevel`); rollup cron (`src/hierarchy/rollup.rs::weighted_rollup`) function→file→module→project→group→workspace |
| v49 | `cross_project_symbol_edges` — opt-in overlay, exact-FQN-only tier, never touches the per-project resolver (zero regression) |
| v51 | categorical-invariant cache (if materialized) |

Inter-project metrics (Ce/Ca/instability/abstractness/distance-from-main-sequence, project×project
DSM, god-projects, cross-project cycles) fuse three provenance-tagged edge sources
(`project_dependencies` primary, `cross_project_similarities` low weight,
`cross_language_signature_clones` lower) — the generic `compute_module_metrics` algorithm
lifted one level. Flagship tools (`workspace_architecture_quality`,
`cross_project_coupling`, `workspace_engineering_scorecard`) read via a shared
`src/hierarchy/reader.rs` and a 6-format `View` renderer, leaving the 30 intra tools
untouched. CVE propagation (`cross_project_cve_exposure`) walks the reverse dependency
closure with decayed severity.

### Item 4 (roadmap): category subsystem

`src/category/` will reify the categories grounded in real tables (Call, FileDep,
ConceptPoset, LockOrder, ProjectDep + the Containment functor chain) and check functor laws:
extensive sums (file counts, cyclomatic totals) are **strict** (composition-preserving — a
mismatch is a data-integrity bug surfaced by `categorical_lint`); intensive means / recomputed
PageRank are **lax** (documented). Formal treatment in `docs/formal/` (Rocq/TLA⁺), validated
with coqc/tlapm/TLC, plus a labeled-sample validation experiment.

## Consequences

- The grouping model + level ladder are the shared spine both later stages build on; landing
  them first keeps each subsequent stage a self-contained, testable slice.
- Link-table grouping (vs a column) is the only choice that supports overlapping memberships
  and group-grain metrics without churn.
- Tested: `GroupKind`/`GroupRole`/`HierLevel` golden parity tests; v46 `step_version_is_stable`.
