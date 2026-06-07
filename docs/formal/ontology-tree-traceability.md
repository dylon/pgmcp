# Ontology Tree Formal Verification Traceability

Status: focused ontology read slice for `ontology_tree`.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `ontology_tree` in the 2-call
cluster. The tool is read-only, but it performs recursive hierarchy traversal,
so the safety obligations are about bounded traversal, cycle suppression,
active concept filtering, and duplicate-free output.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `ontology_tree` | Reject blank subtree roots; reject unknown facets; clamp subtree depth to 1..=50; traverse only active ontology concepts over active `is_a`/`part_of`/`broader` edges; suppress corrupt hierarchy cycles with a visited path; return duplicate-free edges; keep facet mode scoped to same-facet endpoints; remain read-only. | `tla/OntologyTreeScope.tla`; `oracle_ontology_trie_accel`; `oracle_ontology_tools`. |

## Issues Found And Corrected

The subtree wrapper passed a raw `root_concept` string through lookup and
response reporting. It now trims the root and rejects blank values before DB
lookup.

The recursive `concept_descendants` query was depth-bounded, so it terminated,
but it did not track a visited path. In a corrupt hierarchy cycle, the same
logical edge could reappear at a different depth. The traversal also followed
active relations through any active memory entity, not only ontology concepts,
and could recurse through inactive concepts before final filtering removed only
the final row.

Correction: the recursive CTE now:

| Correction | Effect |
| --- | --- |
| Adds active `memory_entities` joins in both anchor and recursive terms. | Inactive endpoints are not traversed. |
| Adds `ontology_concept_meta` joins in both terms. | Non-concept memory entities cannot appear in ontology subtree output. |
| Carries a `path` array seeded with `[root, child]`. | Corrupt cycles cannot revisit an already-seen concept. |
| Applies `SELECT DISTINCT` to output edges. | Pre-existing duplicate active relation rows cannot duplicate payload edges. |

The per-facet hierarchy query also now uses `SELECT DISTINCT` for duplicate-safe
read output.

## Formal Model

`tla/OntologyTreeScope.tla` models subtree and facet modes over a small corrupt
hierarchy containing a valid chain, a cycle back to the root, an inactive
concept edge, and a non-concept edge.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BlankRootsRejected` | Empty subtree roots fail closed. |
| `MissingRootsRejected` | Unknown subtree roots fail closed. |
| `InvalidFacetsRejected` | Unknown facets fail closed. |
| `DepthClamped` / `RowsWithinDepth` | Subtree traversal stays within the effective depth bound. |
| `RowsAreActiveConceptEdges` | Returned edges have active ontology concepts at both endpoints. |
| `NoRootAsOwnDescendant` | Corrupt cycles cannot return the root as its own descendant. |
| `FacetRowsScoped` | Facet mode returns only same-facet endpoint edges. |
| `OutputBounded` | Output remains under the finite facet/subtree bound. |
| `NoDuplicateEdges` | No `(child,parent,relation)` edge appears more than once. |

## Verification Run 2026-06-06

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh OntologyTreeScope.tla
```

Result: 15 distinct states, 22 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test oracle_ontology_trie_accel --build-jobs 1
cargo nextest run -p pgmcp-testing --test oracle_ontology_tools --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
