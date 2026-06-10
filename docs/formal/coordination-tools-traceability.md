# Coordination / Client / A2A Tools Formal Verification Traceability

Status: focused request-boundary slice for the cross-project coordination,
client-tracking, and agent-to-agent (A2A) mailbox tool family.

## Scope

These tools are the multi-agent coordination surface: an MCP client whose build
broke on a dependency opens a worktree-coordination request against the
dependency's live editors; editors respond and are suggested the exact git
moves; agents read each other's dependency edges and liveness; and agents
exchange mailbox messages. The family resolves project display names and, until
this hardening pass, did so without a fail-closed resolver — a duplicate display
name could silently select an arbitrary project row, and blank inputs could
reach the database. This slice adds the `project_id_or_err` fail-closed resolver
(trim, reject blank, reject unknown, reject DUPLICATE) across the family,
`pool_or_err` for a uniform missing-pool error, trimmed/blank-rejected string
params, and `.clamp()` on the client-matrix windows.

The coordination *protocol* state machine (the R↔E exchange plus the System
git-scanner gatekeeper) is modeled separately and in full by
`tla/WorktreeNegotiation.tla` (+ its TLAPS and Rocq companions). The four scope
slices below model only what crosses the *tool* request boundary and complement
that protocol model — `CoordinationScope` in particular cross-references the
protocol's `GatekeeperSafety` for the agent/`resolved` trust boundary.

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `coordinate_dependency_block` | Resolve the dependency (U) name fail-closed; resolve the optional dependent (D) name fail-closed when present and non-blank; reject blank/unknown/duplicate names with no coordination opened; key the opened coordination + asserted edge by the resolved ids. | `tla/CoordinationScope.tla`; `tool_coordination.rs`; `cross_project_graph_fields.rs`; `oracle_coordination.rs`. |
| `coordination_respond` | Accept only the closed agent vocabulary `{accept, decline, moved}` after trimming; reject everything else; never record the System-only `resolved` status (an agent can never self-confirm the restore); the `moved` candidate path opens no unblock of the dependent and is read-only w.r.t. its blocked state. | `tla/CoordinationScope.tla`; `tla/WorktreeNegotiation.tla` (`GatekeeperSafety`); `tool_coordination.rs`; `oracle_coordination.rs`. |
| `suggest_worktree` | Resolve exactly one project id fail-closed, then load the project row (path / stable branch) by that id so a duplicate or blank name can never select an arbitrary row; suggest (never run) git; remain read-only. | `tla/CoordinationScope.tla`; `tool_coordination.rs`; `oracle_coordination.rs`. |
| `project_dependents` | Resolve one project name fail-closed; return exactly the LIVE (`valid_to IS NULL`) reverse dependency edges incident on the resolved id; reject blank/duplicate; bounded/read-only. | `tla/ProjectDepsScope.tla`; `cross_project_graph_fields.rs`. |
| `project_dependencies` | Resolve one project name fail-closed; return exactly the LIVE forward dependency edges incident on the resolved id; reject blank/duplicate; bounded/read-only. | `tla/ProjectDepsScope.tla`; `cross_project_graph_fields.rs`. |
| `active_clients` | Trim the OPTIONAL project filter and treat blank as "no filter" (never an error); the filter only removes rows; group rows by project; read-only. | `tla/ClientTrackingScope.tla`; `tool_active_clients.rs`. |
| `client_project_matrix` | Trim the optional project filter (blank → unfiltered); clamp `since_minutes` to `1..=44_640` and `top_files_per_project` to `0..=50`; group rows by project; read-only. | `tla/ClientTrackingScope.tla`; `tool_active_clients.rs`. |
| `a2a_send_message` | Require a non-blank trimmed `body`; default `kind` to `message`, trim it, and validate against the closed `MessageKind` vocabulary; resolve a `to_project` name fail-closed (duplicate → rejected); require at least one of `{to_session, to_project, to_agent}` to address the message. | `tla/A2aMailboxBoundary.tla`; `tool_a2a_mailbox.rs`. |
| `a2a_inbox` | Require at least one of `{session, project, agent}`; resolve the project filter fail-closed (so the inbox can never silently widen to "no project filter"); key read-marking receipts to the resolved recipient session; deliver a message at most once per recipient per channel (the `(message_id, recipient_session)` receipt upsert). | `tla/A2aMailboxBoundary.tla`; `tool_a2a_mailbox.rs`. |
| `WorktreeNegotiation` (protocol) | The dependent D is unblocked ONLY after the System git scanner observed the dependency U back on its stable branch & clean — never on the Editor agent's `moved` claim alone (the gatekeeper trust boundary); an accepted request eventually unblocks D (liveness). | `tla/WorktreeNegotiation.tla`; `WorktreeNegotiation_proofs.tla` (TLAPS); `rocq/WorktreeNegotiation.v` (Qed). |

## Issues Found And Corrected

1. The coordination/client/A2A tools resolved project display names with a raw
   `SELECT id FROM projects WHERE name = $1`.

   That did not trim the project input and did not fail closed for duplicate
   display names: a duplicate name would resolve to an arbitrary (first) row, and
   blank names reached the database. The opened coordination, suggested worktree,
   dependency-edge query, and addressed message could all be keyed to the wrong
   project.

   Correction: every name-resolving path now uses the shared fail-closed
   `sota_helpers::project_id_or_err` resolver — it trims, rejects blank with
   `invalid_params`, rejects unknown, and rejects DUPLICATE with `invalid_params`.
   `coordinate_dependency_block` resolves both the dependency and (when present
   and non-blank) the dependent through it; `suggest_worktree` resolves to a
   single id and then loads the project row *by id*; `a2a_send_message` /
   `a2a_inbox` resolve the `to_project` / project filter through it.

2. `coordination_respond` accepted an unconstrained response string.

   Correction: the response is trimmed and matched against the closed
   `{accept|accepted, decline|declined, moved}` set; anything else returns
   `invalid_params`. The recorded `CoordinationStatus` is agent-settable only —
   `CoordinationStatus::is_agent_settable()` is false exactly for `Resolved`, so
   no agent path through this tool can self-confirm the dependency restore. Only
   pgmcp's git scanner (`Actor::System`) reaches `resolved`, mirroring the v17
   CI-evidence gatekeeper and the work-item tracker's
   `system_absent_from_judgment_columns` property.

3. `active_clients` / `a2a_inbox` / `a2a_send_message` did not trim optional
   string filters consistently, and `client_project_matrix` accepted unbounded
   window inputs.

   Correction: `active_clients`'s optional project filter is trimmed and a blank
   value is treated as "no filter" (it only removes rows; it is never an error).
   `a2a_send_message` trims `body` (blank → rejected), `kind` (blank → the
   `message` default), and `subject` (blank → absent). `a2a_inbox` trims the
   `agent` and `project` filters and treats blank as absent. `client_project_matrix`
   clamps `since_minutes` to `1..=44_640` (1 minute … 31 days) and
   `top_files_per_project` to `0..=50`.

4. The live-edge contract of the dependency-edge reads was not formally pinned.

   The `dependents_of` / `dependencies_of` store queries already filter
   `valid_to IS NULL` (so a superseded edge is never returned) and key on the
   resolved project id in the correct direction (reverse for dependents, forward
   for dependencies). This slice formalizes that contract so a future schema or
   query change that leaked a closed edge or crossed the direction is caught.

## Formal Model

Each `.tla` slice processes one synthetic request per behavior (the
`CircularDependenciesScope` single-request shape) over a finite set of
Projects that includes a DUPLICATE display-name pair, so the state space stays
small. Outcomes are `{ok, rejected}`. These are terminal-outcome specs that
legitimately deadlock once the single response is computed, so each `.cfg`
declares `CHECK_DEADLOCK FALSE`.

### `tla/CoordinationScope.tla`

`coordinate_dependency_block`, `coordination_respond`, `suggest_worktree`.

| Invariant | Meaning |
| --- | --- |
| `BlankOrDuplicateProjectRejects` | A blank/unknown/duplicate dependency (or unresolvable dependent) rejects `coordinate_dependency_block`, opening no coordination and resolving no project id. |
| `SuggestBlankOrDuplicateRejects` | A blank/unknown/duplicate project rejects `suggest_worktree` and loads no project row. |
| `RespondOnlyClosedVocab` | `coordination_respond` succeeds iff the trimmed response is in the closed `{accept, decline, moved}` set. |
| `AgentNeverResolves` | No agent path through `coordination_respond` ever records the System-only `resolved` status (xref `WorktreeNegotiation.GatekeeperSafety`). |
| `RespondStatusFaithful` | An accepted respond records exactly the trimmed closed-vocab status; a rejected respond records none. |
| `RowLoadedByResolvedId` | `suggest_worktree` loads its row by the single resolved id; an opened coordination is keyed by the resolved dependency id. |
| `RespondPathNeverUnblocks` | The `moved` candidate path opens no unblock of the dependent. |
| `RejectedRequestsInert` | Rejected requests open no coordination, load no row, and open no unblock. |

### `tla/ProjectDepsScope.tla`

`project_dependents`, `project_dependencies`.

| Invariant | Meaning |
| --- | --- |
| `BlankOrDuplicateProjectRejects` | A blank/unknown/duplicate project rejects with no edges and no resolved id. |
| `OnlyLiveEdges` | Every returned edge is a live (`valid_to IS NULL`) edge; a superseded edge never leaks. |
| `DependentsEdgesScoped` | `project_dependents` returns exactly the live REVERSE edges incident on the one resolved id. |
| `DependenciesEdgesScoped` | `project_dependencies` returns exactly the live FORWARD edges incident on the one resolved id. |
| `ResolvedProjectIdReal` | A successful response carries a real resolved project id, never the 0 sentinel. |

### `tla/ClientTrackingScope.tla`

`active_clients`, `client_project_matrix`.

| Invariant | Meaning |
| --- | --- |
| `BlankFilterIsUnfiltered` | A blank (post-trim) project filter is not an error — it returns all rows. |
| `FilterOnlyRemovesRows` | An active filter only removes rows; every returned client belongs to a filter-matched project. |
| `SinceClamped` | `client_project_matrix` clamps `since_minutes` into `1..=44_640`. |
| `TopFilesClamped` | `client_project_matrix` clamps `top_files_per_project` into `0..=50`. |
| `NeverErrorsOnFilter` | The optional project filter never produces an error outcome (no fail-closed path here). |

### `tla/A2aMailboxBoundary.tla`

`a2a_send_message`, `a2a_inbox`.

| Invariant | Meaning |
| --- | --- |
| `SendRequiresBody` | A blank (post-trim) body rejects `a2a_send_message` and sends nothing. |
| `StoredKindClosedVocab` | Any sent message carries a kind from the closed `MessageKind` vocabulary. |
| `KindDefaultsToMessage` | An absent/blank kind defaults to `message` on the stored message. |
| `InvalidKindRejects` | An out-of-vocab kind rejects and sends nothing. |
| `ToProjectFailClosed` | A present non-blank `to_project` name resolves fail-closed (duplicate/unknown → rejected); a stored `to_project` id is a real project id. |
| `SendMustAddress` | A message with no recipient is rejected and sends nothing. |
| `InboxMustAddress` | An `a2a_inbox` call with no recipient address is rejected. |
| `InboxProjectFilterFailClosed` | The inbox project filter resolves fail-closed, so the inbox can never silently widen to "no project filter". |
| `ReceiptKeyedToRecipient` | Read-marking is keyed to the resolved recipient session, never another recipient. |
| `DeliveredAtMostOncePerRecipient` | The `(message_id, recipient_session)` receipt upsert means a message is delivered at most once per recipient per channel. |
| `RejectedRequestsInert` | Rejected requests send no message and write no receipt. |

### `tla/WorktreeNegotiation.tla` (coordination protocol)

The protocol state machine that the tool slices ride on. Modeled and proven
three ways: TLC (finite model-check), TLAPS (`WorktreeNegotiation_proofs.tla`,
a machine-checked inductive-invariant proof via z3/zenon), and Rocq
(`rocq/WorktreeNegotiation.v`, closes with `Qed`, no axioms/admits).

| Property | Meaning |
| --- | --- |
| `GatekeeperSafety` | Whenever the dependent D is unblocked, the System git scanner has observed the dependency U stable — only the scanner's observation clears `dBlocked`. |
| `NoUnblockOnClaimAlone` | The Editor agent's `moved` claim alone never unblocks D; without a scanner observation, D stays blocked (no false unblock on an agent's say-so). |
| `EventuallyUnblocked` | Liveness: under weak fairness on the progress actions, an accepted request eventually unblocks D. |

## Verification Run 2026-06-10

TLC, using the RSS-capped wrapper, run with the exact campaign gate
`cd docs/formal/tla && ../../../scripts/tlc-capped.sh <Name>.tla`:

```bash
cd docs/formal/tla
../../../scripts/tlc-capped.sh CoordinationScope.tla
../../../scripts/tlc-capped.sh ProjectDepsScope.tla
../../../scripts/tlc-capped.sh ClientTrackingScope.tla
../../../scripts/tlc-capped.sh A2aMailboxBoundary.tla
```

Results — each prints `Model checking completed. No error has been found.`:

| Spec | Distinct states | Generated states |
| --- | --- | --- |
| `CoordinationScope` | 14 | 28 |
| `ProjectDepsScope` | 9 | 18 |
| `ClientTrackingScope` | 9 | 18 |
| `A2aMailboxBoundary` | 12 | 24 |

`WorktreeNegotiation` was already TLC-checked (gatekeeper safety + liveness) and
carries a TLAPS deductive proof (`WorktreeNegotiation_proofs.tla`, z3/zenon) and
a Rocq proof (`rocq/WorktreeNegotiation.v`, `Qed`, no axioms/admits).

Rust regressions for this family live in:

```text
pgmcp-testing/tests/tool_coordination.rs        # block/respond/suggest boundary
pgmcp-testing/tests/tool_active_clients.rs       # active_clients / client_project_matrix
pgmcp-testing/tests/tool_a2a_mailbox.rs          # send/inbox boundary + receipts
pgmcp-testing/tests/cross_project_graph_fields.rs# project_dependents/dependencies edges
pgmcp-testing/tests/oracle_coordination.rs       # forthcoming: end-to-end coordination oracle
```

These exercise the same fail-closed resolution (duplicate/blank/unknown
rejection), the closed `coordination_respond` / `MessageKind` vocabularies, the
agent/`resolved` trust boundary, the live-edge direction scoping, the optional
client-filter trim and window clamps, and the at-most-once mailbox delivery that
the TLA+ slices pin abstractly.
