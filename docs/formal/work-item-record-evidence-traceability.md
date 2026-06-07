# Work Item Record Evidence Formal Verification Traceability

Status: focused tracker trust-boundary slice for `work_item_record_evidence`.

## Scope

`work_item_record_evidence` lets an MCP caller attach manual evidence to an
acceptance criterion. It must preserve the trust boundary: MCP evidence is
always `source='manual'`, which is audit material but not trusted evidence for
the gatekeeper.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_record_evidence` | Reject invalid criterion ids, verdicts, coverage counts, commit SHAs, and detail JSON before insert; missing criteria write nothing; successful calls insert exactly one manual evidence row; the evidence-recorded counter increments only after insert; no trusted evidence source or work-item status transition is reachable through MCP evidence recording. | `tla/WorkItemRecordEvidenceBoundary.tla`; `work_items_smoke` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The tool forced `source='manual'`, but it did not bound `detail_json`, did not
validate coverage counts before insert, and incremented
`work_item_evidence_recorded` before validation and insert success.

The wrapper now rejects nonpositive criterion ids, invalid verdicts, negative
coverage values, `coverage_count > coverage_total`, oversized detail JSON,
oversized commit SHAs, and malformed JSON before insert. The evidence counter
now increments only after the evidence row is inserted.

## Formal Model

`tla/WorkItemRecordEvidenceBoundary.tla` models local validation, missing
criterion rejection, successful manual evidence insertion, post-insert counter
updates, and the no-self-verification trust invariant.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `LocalInvalidBeforeLookup` | Invalid local fields reject before criterion lookup or insert. |
| `MissingCriterionWritesNothing` | A valid request for a missing criterion inserts no evidence. |
| `RejectedWritesNothing` | Rejected calls insert no evidence and do not increment the evidence counter. |
| `AcceptedWritesManualEvidenceOnly` | Successful MCP calls insert exactly one manual evidence row. |
| `CounterAfterInsert` | The evidence counter advances only after an inserted row. |
| `NoTrustedEvidenceFromMcp` | MCP recording cannot produce trusted-source evidence. |
| `NoSelfVerification` | Recording evidence does not change work-item status. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh WorkItemRecordEvidenceBoundary.tla
```

Result: 11 distinct states, 21 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke --build-jobs 1
```

Result: pending until sibling dependency compilation is restored.
