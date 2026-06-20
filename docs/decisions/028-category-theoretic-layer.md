# ADR-028: Category-theoretic layer over the workspace graph

- **Status:** Accepted — **full functor toolset + formal proofs delivered + validated.**
- **Date:** 2026-06-19
- **Relates to:** item 4 (category theory over the workspace), ADR-027 (hierarchical rollup).
  Module: `src/category/`. Tools: `categorical_lint`, `functorial_impact`, `common_dependency`
  (pullback), `integration_point` (pushout), `effect_functor`, `naturality_gap`, `colimit_view`.
  Formal: `docs/formal/containment_functor.v` (Rocq, coqc-verified), `ContainmentFunctor.tla`
  (TLC: no error). Experiment: `docs/experiments/item4-categorical-validation.md`.

## Context

The user asked whether category theory can be **constructed over the workspace** and **applied
to software-engineering intelligence** — not as decoration, but as a tool that surfaces real
issues. The risk with "category theory over code" is vacuity: abstract structure that proves
nothing actionable. The design constraint is therefore that every categorical claim be
**grounded in a real table** and **falsifiable** (it can flag an actual bug).

## Decision

Model the workspace as a small set of categories whose objects and morphisms are existing
rows:

| Category        | Objects                       | Morphisms                              |
|-----------------|-------------------------------|----------------------------------------|
| **Call**        | functions (`file_symbols`)    | `symbol_references` call edges         |
| **FileDep**     | files (`indexed_files`)       | `import` edges                         |
| **ProjectDep**  | projects                      | `project_dependencies` (multi-ecosystem) |
| **Containment** | the `HierLevel` chain         | the rollup functor symbol⊳…⊳workspace  |

The **Containment functor** is exactly ADR-027's rollup: it carries metrics up
`symbol → function → file → module → project → group → workspace`. A functor must preserve
composition. Whether a *metric* is preserved depends on its kind:

- **Extensive** metrics (counts — `file_count`) roll up by **addition**. Composition is
  preserved exactly: `total_workspace == Σ_projects`. This is `RollupLaw::Strict`.
- **Intensive** metrics (means — instability, distance) roll up by **averaging**. Composition
  is only approximately preserved (an average of averages ≠ the global average unless
  weighted). This is `RollupLaw::Lax` — reported honestly, never asserted.

`categorical_lint` checks the **strict** laws as data-integrity invariants: the workspace
total of each extensive column must equal the sum over `project_metrics`. A mismatch means
the rollup lost or double-counted — a real bug, caught categorically. This is the concrete,
falsifiable payoff: the functor laws are not decoration, they are assertions about the data.

## The wider functor toolset (delivered)

- `functorial_impact` — where the intensive (lax) rollup diverges from a size-weighted mean
  (collapsing the level misleads). ✅
- `common_dependency` (pullback) / `integration_point` (pushout) over `ProjectDep`. ✅
- `naturality_gap` — divergence between the `import` and `semantic` functors on file pairs
  (structurally coupled but conceptually distant = erosion). Co-change is the third functor,
  addable when a co-change table is materialized. ✅
- `effect_functor` — `Call → effect-set monoid` (the effect-monoid generators + most effectful
  symbols). ✅
- `colimit_view` — `memory_unified_edges`/`_nodes` as a colimit of its per-source diagrams. ✅
- Formal treatment in `docs/formal/`: `containment_functor.v` (Rocq) proves the strict-sum
  functor law for all finite hierarchies (coqc exit 0, no admits); `ContainmentFunctor.tla`
  model-checks it with TLC (no error). Validation experiment:
  `docs/experiments/item4-categorical-validation.md`.

## Consequences

- Category theory earns its place by being **falsifiable**: `categorical_lint` flags genuine
  rollup-integrity bugs, and the strict/lax split is an honest statement of which abstractions
  hold exactly vs approximately.
- Grounding every category in a real table avoids the vacuity trap.
- Tested: `RollupLaw` roundtrip + `STRICT_LAWS` shape (unit); `categorical_lint` real-DB test
  asserts the law passes on a consistent rollup and is flagged on a corrupted one.
