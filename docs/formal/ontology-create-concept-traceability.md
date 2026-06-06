# Ontology Create Concept Formal Verification Traceability

Status: focused high-use ontology-authoring slice for
`ontology_create_concept`.

## Scope

30-day durable telemetry ranked `ontology_create_concept` among the next
highest-use tools without a formal ledger row after the search, tracker, and
memory retrieval slices.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `ontology_create_concept` | Trim and reject blank names; reject oversized names before DB writes; trim and validate facets against the closed vocabulary; serialize concurrent same-name creation with one transaction-local advisory lock; create at most one active concept row per normalized name; keep agent-authored concepts candidate-only; preserve curator-set canonical metadata; return actual persisted facet/status rather than the requested values when an existing concept is reused. | `tla/OntologyCreateConceptAtomicity.tla`; `pgmcp-testing/tests/oracle_ontology_tools.rs`. |

## Issues Found And Corrected

`create_concept` used a `SELECT` followed by `INSERT` without a uniqueness
constraint or lock over active `(name, concept)` rows. Concurrent tool calls
could create duplicate active concept entities. Correction: concept creation now
normalizes the name, opens a short transaction, takes exactly one
`pg_advisory_xact_lock` keyed by the normalized concept name, rechecks for an
active row, inserts only if absent, and upserts metadata before commit.

The MCP wrapper accepted blank names and unbounded concept-name strings.
Correction: shared name normalization rejects blank and >256-character names
before reaching SQL. The facet parser now trims whitespace and rejects blank
facets explicitly.

The tool always reported `status: "candidate"` and the requested facet even when
it reused an existing curated concept whose metadata could not be overwritten by
the curation-safe `ON CONFLICT ... WHERE status = 'candidate'` clause.
Correction: the tool fetches `ontology_concept_meta` after creation/reuse and
returns the actual persisted facet and status.

## Concurrency Boundary

The write path takes one PostgreSQL transaction-level advisory lock and no
process mutexes. There is no nested lock acquisition and no lock-order cycle:
each call either rejects before the transaction or holds the per-name lock for
the `resolve-or-insert + metadata-upsert` transaction, then releases it at
commit/rollback.

## Formal Model

`tla/OntologyCreateConceptAtomicity.tla` models two workers racing on one valid
normalized name, malformed name/facet requests, an existing canonical concept,
and an existing candidate concept.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsDoNotWrite` | Malformed name/facet requests reject without changing concept state. |
| `NoDuplicateActiveConcept` | Concurrent valid creates never produce more than one active concept row. |
| `SingleLockOwner` | At most one worker holds the per-name advisory lock. |
| `DoneWorkersReleaseLock` | Completed workers do not retain the lock. |
| `AgentCreatesCandidateOnly` | Agent-created rows are candidate concepts only. |
| `ExistingCanonicalPreserved` | Agent retries cannot overwrite canonical curator metadata. |
| `ResponseReflectsPersistedMeta` | Successful responses report the stored metadata. |
| `SuccessfulResponsesUseNormalizedName` | Successful responses use the trimmed bounded name. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh OntologyCreateConceptAtomicity.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 41 distinct states, 47
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test oracle_ontology_tools --build-jobs 1
```

Result: 6/6 passed. The focused run covers blank/oversized-name rejection,
blank facet rejection, facet/name trimming, actual metadata in the MCP response
for existing canonical concepts, same-name concurrent creation, direct
query-layer create/search behavior, candidate-only agent invariant assertions,
and hierarchy-edge resolution.
