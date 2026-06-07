# Work Item Experiment Link Formal Verification Traceability

Status: focused tracker/experiment bridge slice for `work_item_link_experiment`.

## Scope

The tracker/experiment bridge lets a scientific experiment become a normal work
item with priority, tags, claiming, and evidence-gated verification. This slice
verifies the pgmcp transaction boundary around that bridge, especially the
auto-create and acceptance-criterion seed path.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_link_experiment` | Reject invalid slugs, hypothesis ids, and oversized titles before writes; resolve experiments before opening a write transaction; commit the optional tracking item, bridge row, and seeded `experiment_verdict` criterion atomically; roll back every visible write on bridge/criterion failure; increment the create counter only after commit; serialize concurrent criterion seeding for the same existing work item; avoid lock-order deadlocks. | `tla/WorkItemLinkExperimentAtomicity.tla`; `work_items_smoke` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The bridge previously performed several writes through separate helper calls.
If auto-create succeeded and a later bridge or criterion write failed, callers
could observe a partially-created tracking item without the experiment bridge.

The tool now opens one transaction for existing-item lookup or auto-create,
bridge upsert, criterion lookup, and criterion insert. The DB helpers have
in-transaction variants so the public helper wrappers remain usable while this
tool commits the whole unit atomically.

Formal review also exposed a concurrency hazard in the existing-work-item path:
the lookup used `FOR SHARE`, so two concurrent link calls could both observe no
existing `experiment_verdict` criterion and insert duplicates. The lookup now
uses `FOR UPDATE`, serializing only calls that target the same work item before
the lookup-then-insert seed step.

The in-memory `work_items_created` stat was also incremented immediately after
the auto-created row insert. That row could still roll back if a later bridge or
criterion write failed. The counter now increments only after transaction
commit.

## Formal Model

`tla/WorkItemLinkExperimentAtomicity.tla` models both the single-call request
boundary and a two-transaction race on the same existing work item.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `RejectedWritesNothing` | Rejected calls have no visible item, bridge, or criterion writes. |
| `LocalValidationBeforeTx` | Local slug/title/hypothesis validation rejects before any transaction starts. |
| `ExperimentResolvedBeforeTx` | Missing experiments reject before a write transaction starts. |
| `TransactionFailuresRollback` | Missing items and bridge/criterion failures roll back all visible writes. |
| `SuccessfulLinksCommitBridge` | Every successful link publishes a bridge row atomically with its companion writes. |
| `AutoCreateAtomic` | Auto-create success publishes exactly the item, bridge, and seeded criterion. |
| `ExistingLinksNeverCreateItems` | Linking an existing item never creates a new item and uses an update lock before seeding. |
| `ExistingCriterionSeedIdempotent` | Existing experiment-verdict criteria are reused rather than duplicated. |
| `CreateCounterAfterCommit` | The work-item create counter increments only after a committed auto-create. |
| `NoWaitCycle` | The same-item seed race has no lock wait cycle. |
| `NoDuplicateConcurrentCriterionSeeds` | Two concurrent same-item link calls can seed at most one criterion. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkItemLinkExperimentAtomicity.tla
```

Result: 204 distinct states, 402 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: pending until sibling dependency compilation is restored.
