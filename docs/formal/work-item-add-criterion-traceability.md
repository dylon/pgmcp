# Work Item Add Criterion Formal Verification Traceability

Status: focused tracker trust-boundary slice for `work_item_add_criterion`.

## Scope

`work_item_add_criterion` lets an agent attach machine-checkable acceptance
criteria to a work item. That is useful, but it must remain below the evidence
trust boundary: creating a criterion is not evidence and cannot verify work.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_add_criterion` | Validate criterion kind, coverage mode, and gate against closed vocabularies before insert; reject blank or oversized descriptions and oversized acceptance URIs before lookup/insert; normalize optional URI/gate/coverage fields; missing items write nothing; successful calls insert exactly one criterion and no evidence; status remains unchanged. | `tla/WorkItemAddCriterionBoundary.tla`; `work_items_smoke` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool previously delegated `criterion_kind`, `coverage_mode`, and `gate`
validation to DB CHECK failures and did not bound description or acceptance URI
sizes. It also kept whitespace in optional `coverage_mode`, `gate`, and
`acceptance_uri` fields.

The wrapper now validates the closed vocabularies before any insert attempt,
caps descriptions at 4096 bytes and acceptance URIs at 2048 bytes, normalizes
blank optional fields to absent values, and returns normalized response fields.

## Formal Model

`tla/WorkItemAddCriterionBoundary.tla` models local validation, missing-item
lookup failure, the successful insert, and the trust invariant that the tool
does not create evidence or change work-item status.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `LocalInvalidBeforeLookup` | Bad kind/coverage/gate and oversized or blank text reject before item lookup. |
| `MissingItemWritesNothing` | A valid request targeting a missing item performs lookup but inserts no criterion. |
| `RejectedWritesNothing` | Rejected calls create no criteria, no evidence, and no status change. |
| `AcceptedWritesOneCriterionOnly` | A valid call writes exactly one criterion and no evidence/status change. |
| `NoSelfVerification` | The tool cannot verify work by adding a criterion. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkItemAddCriterionBoundary.tla
```

Result: 9 distinct states, 17 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: pending until sibling dependency compilation is restored.
