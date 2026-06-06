# Work Item Claim Formal Verification Traceability

Status: focused tracker collaboration slice for `work_item_claim`.

## Scope

The refreshed 31-day `mcp_tool_calls` ranking shows `work_item_claim` at 3
calls. The tool is a write-side collaboration primitive: a caller claims one
work item through a transactional row compare-and-set, writes a claim ledger row,
and refreshes agent presence after a successful claim.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `work_item_claim` | Validate explicit agent ids before writes; resolve the public id before mutation; atomically claim only when the item is unowned, already owned by the same agent, or owned by an expired lease; reject active-owner contention, unresolved dependencies, and terminal statuses without writes; clamp lease seconds; write item and claim-ledger state atomically; touch presence only after successful claims; allow at most one winner in a two-agent race. | `tla/WorkItemClaimAtomicity.tla`; `oracle_work_item_claim`; filtered `work_items_smoke`. |

## Issues Found And Corrected

Explicit whitespace `agent_id` values were accepted as real owner ids because
the shared helper checked only `is_empty()` and did not trim first.

Correction: `agent_of` now trims before applying the fallback sentinel, and
`work_item_claim` rejects an explicitly supplied blank `agent_id` before
resolving or mutating the item.

## Formal Model

`tla/WorkItemClaimAtomicity.tla` models the MCP agent-id boundary, lease clamp,
single-row CAS ownership cases, unresolved-dependency and terminal no-write
paths, status promotion to `in_progress`, item/ledger/presence atomicity for
successful claims, row-lock release, and a two-agent race outcome.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidAgentNoWrite` | Blank explicit agent ids reject without item, ledger, or presence writes. |
| `ContendedOwnerNoWrite` | Active ownership by another agent produces a contended response without writes. |
| `BlockedNoWrite` | Unresolved dependencies prevent a claim without writes. |
| `TerminalNoWrite` | Terminal statuses cannot be claimed or mutated. |
| `SuccessfulClaimAtomic` | Successful claims update owner, ledger, presence, and claim count together. |
| `LeaseClamped` | Effective lease seconds stay within `10..=86400`. |
| `OpenStatusPromoted` | Open claimable statuses promote to `in_progress`. |
| `InProgressStaysInProgress` | Reclaiming an already in-progress item does not regress status. |
| `ExpiredLeaseStealable` | Expired leases can be taken by a new agent. |
| `SameOwnerRenewalAllowed` | The current owner may renew by claiming again. |
| `ConcurrentAtMostOneWinner` | A two-agent race cannot produce two successful claimants. |
| `ConcurrentLedgerMatchesWinner` | The race writes one claim ledger row exactly when one agent wins. |
| `ConcurrentFinalOwnerIsWinner` | The final owner in the race is the winning agent. |
| `RowLockReleased` | No response path leaves the modeled row lock held. |

## Verification Run 2026-06-06

```bash
cargo nextest run -p pgmcp-testing --test oracle_work_item_claim --build-jobs 1
```

Result: 2/2 passed for explicit blank-agent rejection, no-write failure, trimmed
agent ownership, claim ledger, and presence behavior.

```bash
cargo nextest run -p pgmcp-testing --test work_items_smoke \
  work_item_claim_concurrency_and_handoff --build-jobs 1
```

Result: 1/1 passed for the existing concurrent-claim, owner-gated release,
handoff, and claim-next smoke path.

```bash
env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M PGMCP_TLC_METASPACE=64m \
  PGMCP_TLC_CLASS_SPACE=32m PGMCP_TLC_CODE_CACHE=128m \
  ../../../scripts/tlc-capped.sh WorkItemClaimAtomicity.tla
```

Result: TLC exit 0; 14 distinct states, 28 states generated; no invariant
violations.
