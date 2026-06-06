# Work Item Search/Ingest/Bulk Formal Verification Traceability

Status: focused high-use tracker slice for `work_item_search`,
`work_item_ingest_plan`, and `work_item_bulk`.

## Scope

These tools cover tracker retrieval, plan ingestion, and multi-item mutation.
Their correctness boundary is a mix of fail-closed scoping, bounded resource
use, all-or-nothing ingestion, and preservation of the tracker trust boundary.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_search` | Trim nonempty queries; trim optional project filters; reject unknown or ambiguous explicit projects; reject non-1024d query embeddings before pgvector; clamp result limits to 1..100; return only hits in the resolved project scope. | `tla/WorkItemSearchIngestBulk.tla`; `pgmcp-testing/tests/work_items_smoke.rs`. |
| `work_item_ingest_plan` | Reject empty/unrecognized plans; cap parsed nodes before writing; resolve explicit projects strictly; normalize optional definition slugs; upsert all plan nodes and first-time acceptance criteria in one transaction; lock parents while deriving child `root_id`; roll back the whole ingest on any DB failure. | `tla/WorkItemSearchIngestBulk.tla`; `pgmcp-testing/tests/work_items_smoke.rs`. |
| `work_item_bulk` | Normalize operation names; require a target selector; trim, reject blank, and dedupe explicit public IDs before mutation; bound the raw target list; validate reprioritize bounds before mutation; keep `set_status` routed through `queries::set_work_item_status` as `Actor::Agent`; preserve partial-success accounting for per-item legal/illegal transition splits. | `tla/WorkItemSearchIngestBulk.tla`; `pgmcp-testing/tests/work_items_next_action_smoke.rs`. |

## Issues Found And Corrected

`work_item_search`, `work_item_reprioritize`, and `work_item_ingest_plan` used
the query-layer `resolve_project_id` helper, which treats an unknown project
name as workspace-global. Correction: these tool boundaries now use the strict
existing-project resolver already used by `work_item_create`, so explicit
unknown or duplicate project names fail closed.

`work_item_search` passed whatever embedding length the configured backend
returned into a `vector(1024)` query. Correction: the tool now rejects non-1024d
query embeddings before pgvector execution and echoes the normalized limit.

`work_item_ingest_plan` wrote nodes and acceptance criteria in a per-row loop
without a transaction. Correction: ingestion now begins one transaction, upserts
all nodes and first-time criteria through transaction helpers, and commits only
after the parsed tree has been persisted. A regression trigger forces a
criterion insert failure after earlier rows would have been written; the test
asserts that no item or criterion rows remain.

`work_item_ingest_plan` had no parsed-node cap. Correction: it now rejects plans
above `MAX_INGEST_NODES` before any DB write.

`work_item_bulk` accepted blank explicit IDs via the downstream lookup error,
processed duplicate public IDs repeatedly, and accepted out-of-range
reprioritize values until the DB constraint rejected them. Correction: explicit
IDs are trimmed, blank entries fail before mutation, duplicates are attempted
once, and priority bounds are checked at the tool boundary.

## Concurrency Boundary

This slice introduces no new mutexes and no spawned worker threads. Ingestion
uses one PostgreSQL transaction; parent rows are read under `FOR SHARE` before
child/root derivation, matching the tracker create path. A failure inside the
transaction releases all DB locks and rolls back all node and criterion writes.

Bulk preserves the existing concurrency design: lifecycle changes still pass
through `set_work_item_status`, whose row-lock transition recheck and append-only
history write are verified in `work-item-set-status-traceability.md`. Bulk does
not hold a process-level lock across target mutations; it preflights shared
inputs and then applies the per-item chokepoint.

## Formal Model

`tla/WorkItemSearchIngestBulk.tla` models representative valid and invalid
requests for search, ingest, and bulk. The model abstracts database contents
into normalized response fields so TLC explores a small finite state space while
checking the boundary contracts directly.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `SearchInvalidRejected` | Blank queries, unknown projects, and bad embedding dimensions reject. |
| `SearchLimitBounded` | Search limits are always in 1..100. |
| `SearchEmbeddingDimGuard` | Successful search requests have 1024d embeddings. |
| `SearchHitsScoped` | Successful search hits are scoped to the normalized project filter. |
| `IngestInvalidOrFailedWritesNothing` | Invalid or failed ingests write no items or criteria. |
| `IngestOversizeRejectedBeforeWrite` | Oversized plans reject before mutation. |
| `IngestDbFailureRollsBack` | DB failure inside ingestion leaves zero committed writes. |
| `IngestAtomicWrites` | Successful ingestion commits node and criterion writes together. |
| `IngestParentRootLocked` | Successful child ingestion derives root ids under a parent lock. |
| `BulkInvalidRejectedBeforeMutation` | Invalid bulk requests attempt and mutate no targets. |
| `BulkDedupesTargets` | Duplicate explicit public IDs collapse to one attempted target. |
| `BulkPriorityBounds` | Successful reprioritize uses a priority in 0..100. |
| `BulkStatusActorIsAgent` | Successful bulk status changes use agent authority only. |
| `BulkPartialSuccessAccounting` | Bulk partial success reports `succeeded + failed = attempted`. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh WorkItemSearchIngestBulk.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 17 distinct states, 34
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke \
  --test work_items_next_action_smoke --build-jobs 1
```

Result: 14/14 passed. The focused run covers search fail-closed scoping,
embedding-dimension rejection, oversized-plan rejection, transaction rollback
on criterion-insert failure, existing tracker concurrency smoke tests, and bulk
target normalization/priority preflight behavior.
