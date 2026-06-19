# ADR-023: Agent feedback, voting, bug assert-and-close, and data-table linkage

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-003 (closed-vocabulary idiom), ADR-004 (work-item tracker trust
  boundary), ADR-010 (JSON data tables). Migrations: v43 (`agent_feedback`, `votes`), v44
  (`data_table_links`, bug reproduction-criterion columns).

## Context

Four gaps in the agent-facing surface:

1. **No feedback channel.** Agents/MCP clients had no way to tell pgmcp what they like,
   dislike, or want feature-wise. Such signal was lost to chat.
2. **Agents file bugs but cannot close them.** An agent can open a `kind='bug'` work-item
   and fix it in code, but `verified` is Gatekeeper-only, gated on trusted evidence
   (`source IN ('ci','stop_hook','subagent_audit','external_auditor','user_signoff',
   'experiment')`); `work_item_record_evidence` forces `source='manual'`. With no CI,
   hook, or experiment wired, a fixed bug stayed open forever — the reported "agents file
   bug reports but never resolve them."
3. **No voting.** No way for agents to express collective priority on issues/feedback.
4. **Data tables had no association** to the experiment or work-item they back — only a
   `project_id` scope.

## Decision

### 1. `agent_feedback` (v43) — a standalone agent-voice channel

A dedicated table (not the work-item tracker: feedback is agent *voice*, not project
*work*), with closed vocabularies (ADR-003 idiom — enum + `sql_in_list()` CHECK + golden
test) in `src/feedback/`:
`FeedbackCategory{complaint, feature_request, praise, bug_report, question, suggestion}`,
`FeedbackSentiment{strongly_negative…strongly_positive}` (5-point),
`FeedbackStatus{open, acknowledged, planned, resolved, declined}`. Rows are embedded on
write (like `work_items`) for semantic recall. Tools: `submit_feedback`, `list_feedback`,
`search_feedback` (fts/semantic/hybrid), `respond_feedback` (triage), and
`promote_feedback_to_work_item` (a one-way seam into the tracker, idempotent).

### 2. `votes` (v43) — generic one-vote-per-agent ledger

A single generic table over any votable entity:
`votes(target_type, target_id, agent_id, direction, weight, …)` with
`UNIQUE(target_type, target_id, agent_id)` — the integrity mechanism behind "at most one
vote per issue per agent". `VoteTargetType{work_item, feedback, bug, experiment}`,
`VoteDirection{up, down}`. `agent_id` is the MCP caller's declared `clientInfo.name` (via
`extract_caller`, the work-item tracker's identity primitive), injected by the `#[tool]`
wrapper when omitted. **Honest limitation:** `agent_id` is *identification*, not
cryptographic *authentication* — a client could spoof its `clientInfo.name`. The UNIQUE
constraint is the integrity boundary; a stronger per-environment `vote_token` is a
deliberate future seam (off by default). Tools: `cast_vote` (idempotent upsert — re-voting
updates), `retract_vote`, `tally_votes` (up/down/net-weight/voters, joinable for ranking).

### 3. `work_item_assert_fixed` (v44 columns) — close-the-loop without breaking trust

The fix for gap 2 is modeled on the experiment-conclude path: the agent's claim is
*checked*, not trusted. v44 adds machine-checkable criterion columns to
`work_item_bug_details` (`verification_command`, `expected_signal`, `criterion_locked_at`).
`work_item_assert_fixed`:

1. requires a `kind='bug'` item and a non-empty `verification_command` (a check that fails
   before the fix and passes after);
2. **freezes** that criterion (`criterion_locked_at` set once — anti-tamper, mirroring an
   experiment hypothesis's locked criterion);
3. walks the **agent-legal** part of the path (`→ claimed_done` as `Actor::Agent`);
4. reports the remaining trusted step.

It **never** calls a Gatekeeper transition: `verified` is reachable only via the existing
trusted seams — CI posting the `verification_command`'s result (`POST
/api/tracker/ci_evidence` with the tracker user_token) or a decided bug-fix experiment,
after which the gatekeeper flips it. The ADR-004 trust boundary is preserved (an
`Actor::Agent` has no arm into a judgment state); the new tool is purely a *guard +
convenience* that gives the previously-missing self-service "I fixed it" affordance.

### 4. `data_table_links` (v44) — the missing association

A generic bridge `data_table_links(table_id, target_type, target_id, role,
UNIQUE(table_id, target_type, target_id))` mirroring the `work_item_experiment` bridge,
with `LinkTargetType{experiment, work_item}`. Tools `data_table_link` / `data_table_unlink`
(the latter verifying the target exists, since the bridge is polymorphic and carries no
DB-level FK to experiments/work_items); links are surfaced in `data_table_describe`.

## Consequences

- **Positive:** agents have a structured feedback voice; collective priority via votes; a
  trust-preserving close-the-loop for bugs they fix; and benchmark/measurement tables can
  be tied to the experiment/work-item they back.
- **Neutral / honest:** `agent_id`-based identity is spoofable (the `vote_token` seam
  addresses environments needing more); `work_item_assert_fixed` advances only to
  `claimed_done` — final closure still requires trusted CI/experiment evidence (by
  design).
- **Tested:** real-DB lifecycle tests (`feedback_votes_lifecycle`,
  `data_table_links_lifecycle`, `work_item_assert_fixed`, `bug_gate_query`) exercise every
  new dispatched tool through `call_tool_cli` (also satisfying the Layer-D coverage gate),
  plus vocab golden tests and migration `step_version_is_stable` tests.
