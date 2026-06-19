# ADR-028: Category-theoretic layer over the workspace graph

- **Status:** Accepted (strict-law lint landed; the wider functor toolset is the roadmap below)
- **Date:** 2026-06-19
- **Relates to:** item 4 (category theory over the workspace), ADR-027 (hierarchical rollup).
  Module: `src/category/`. Tool: `categorical_lint`.

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

## Roadmap (the wider functor toolset)

- `functorial_impact` — where collapsing a level breaks a lax law badly enough to mislead.
- `common_dependency` (pullback) / `integration_point` (pushout) over `ProjectDep`.
- `naturality_gap` — divergence between the `import`, `co_change`, and `semantic` functors
  on the same objects (hidden coupling / architectural erosion); reuses
  `cochange_mutual_information`, now computable at project grain.
- `effect_functor` — `Call → effect-set monoid` (reuses `effect_propagation`).
- `colimit_view` — formalizes `memory_unified_edges` as a colimit.
- Formal treatment in `docs/formal/` (Rocq/TLA⁺) proving the functor laws, validated with
  coqc/tlapm/TLC, plus a labeled-sample validation experiment that the categorical invariants
  surface real, otherwise-undetected issues.

## Consequences

- Category theory earns its place by being **falsifiable**: `categorical_lint` flags genuine
  rollup-integrity bugs, and the strict/lax split is an honest statement of which abstractions
  hold exactly vs approximately.
- Grounding every category in a real table avoids the vacuity trap.
- Tested: `RollupLaw` roundtrip + `STRICT_LAWS` shape (unit); `categorical_lint` real-DB test
  asserts the law passes on a consistent rollup and is flagged on a corrupted one.
