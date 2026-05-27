# ADR-004: Work-Item / Plan Tracker + A2A Collaboration

- **Status:** Accepted (implemented 2026-05-27)
- **Supersedes:** none
- **Related:** ADR-003 (tag-set over enum), the serene-eclipse "Agent Completion
  Enforcement" cluster (`~/.claude/plans/i-need-a-way-serene-eclipse*.md`), the
  experimentation-management subsystem (`docs/experiments/README.md`).
- **Design plan:** `~/.claude/plans/plan-mcp-support-for-moonlit-dongarra.md`

## Context

pgmcp was a read-mostly indexer: it could *analyze* code (TODO/FIXME scans,
tech-debt scoring) and *advise* (session mandates, memory graph), but had **no
first-class place to record, query, (re)prioritize, and verify work items** with
a lifecycle. The only write paths (`memory_*`, `session_mandates`) model *facts*
and *directives*, not *work*.

This ADR records the design of a tracker that lets users and agents record
tasks/todos/fixmes/ideas/brainstorms/notes/questions/action-items/goals/epics/
plans/sub-tasks/nice-to-haves/experiments (14 kinds) in an **arbitrary-depth
tree**, validate
plans against reusable definitions, gate "done → verified" on machine-checkable
evidence an agent **cannot fabricate**, auto-ingest agent plans into the tree,
and coordinate multiple agents over a shared plan with live presence + an
activity feed.

## The load-bearing decision: trust boundary lives in the state machine

An agent's word is not trusted. This is enforced *structurally*, not by
convention, in `src/tracker/transition.rs`:

- Status lifecycle (10 states): `pending → ready → in_progress → claimed_done →
  verifying → verified`, plus `blocked`, `rejected`, `deferred`, `cancelled`.
- `claimed_done` is the agent's self-report and is **explicitly not trusted**;
  `verified` is the only "done" the roll-up counts.
- `check_transition(from, to, actor, ctx)` is the single chokepoint, called by
  `set_work_item_status` before any `UPDATE`, writing the append-only
  `work_item_status_history` row in the same transaction. The crux rules:
  1. **`→verified` is `Gatekeeper`-only**, from `claimed_done`/`verifying`, and
     only with a passing `verification_evidence` row from a **trusted source ≠
     the agent**. There is **no `Agent` arm into `verified` anywhere** in the
     matrix — an agent cannot self-verify.
  2. **`→deferred` is `User`-only** and requires a `scope_negotiations` row — an
     agent cannot self-defer.
  3. **`→rejected` is `Gatekeeper`-only** (failing evidence) — an agent cannot
     mark its own work rejected to dodge re-verification.

Actors: `User | Agent | Gatekeeper | System`. The `Gatekeeper`/`System` arms are
reachable only server-side (CI/Stop-hook/auditor/experiment engine via the
credential-gated REST endpoint or the experiment engine), never from an MCP tool
whose caller is an agent.

**Why hard to game:** the agent authors code and *claims* `claimed_done` but
cannot (1) fake an exit code (produced by an external runner, posted with
`source≠agent`), (2) fake an external-auditor verdict, (3) self-verify (no Agent
arm), (4) self-defer (no Agent arm + `scope_negotiations.actor_kind='user'`), or
(5) satisfy a universal clause with one case. MCP-posted evidence is forced to
`source='manual'`, which does **not** satisfy the trusted-source set.

## Data model

Two versioned migrations + one guarded late bridge:

- **`v4_work_items`** (`WORK_ITEMS_V1 = 4`) — 12 tables: `work_items` (self-FK
  tree + denormalized `root_id`, `kind`/`status`/`origin` CHECKs, parametric
  cols, `embedding vector(1024)`, `due_at`/`snooze_until`),
  `work_item_status_history`, `tags` + `work_item_tags`, `work_item_progress`,
  `plan_definitions` + `definition_rules`, `acceptance_criteria` (`gate` col) +
  `verification_evidence` (`source` includes `'experiment'`; `criterion_kind`
  includes `'experiment_verdict'` — both forward-declared for Phases 8/10),
  `item_relations`, `work_item_code_anchor`, `scope_negotiations`.
- **`v5_work_items_collab`** (`WORK_ITEMS_COLLAB_V1 = 5`) — claim columns on
  `work_items` (`claimed_by`/`claimed_at`/`lease_expires_at`/`claim_count`),
  `work_item_claims` ledger, `agent_presence`, `agent_identity` view.
- **`ensure_work_item_experiment_bridge`** — late inline `ensure_*` (after
  `ensure_experiment_tables`), guarded by a `to_regclass` preflight so a partial
  install of either subsystem can't break migrations.

Closed dimensions are `TEXT` + `CHECK` + a closed Rust enum (ADR-003 idiom;
`src/tracker/kind.rs`, `status.rs`); the CHECK is built from the enum's
`sql_in_list()` so the Rust arms, the DB CHECK, and the validator's vocabulary
are a single source of truth (referential-integrity tests enforce it). The
kind/status/origin CHECKs are re-applied **unconditionally** on every startup
(idempotent DROP+ADD via `install_work_items_checks`), so adding a vocabulary
value — e.g. the `brainstorm` kind (a container that groups loosely-captured
`idea` children for triage) — is a constraint swap that lands on existing
installs too, not only fresh ones gated behind the v4 version flag. Tags are an
open catalog + join (user-extensible, shared, many-to-many).

## Completion roll-up

Recursive CTE over `parent_id` (`src/tracker/rollup.rs`). **Only `verified`
leaves count** toward `verified_fraction` (the gate); `claimed_fraction` is
advisory ("the agent thinks it's this done"). `deferred`/`cancelled` leaves are
excluded from numerator and denominator. Universal/parametric clauses
(anti-single-case): a `parametric=TRUE` item is done iff its `universal`
criterion has evidence with `coverage_count ≥ parametric_expected` AND
verdict=pass — one passing case ⇒ not done.

## Plan definitions & validation

`plan_definitions` + typed `definition_rules` (closed `rule_kind` vocabulary, one
checker each, in `src/tracker/validate.rs`). `plan_validate` walks a subtree and
returns the `architecture_violations` report shape (severity-sorted). Validation
is *advisory*; the *hard* gate is the verified-status transition.
`plan_definition_export`/`_import` round-trip a serene-eclipse-shaped TOML
(`[definition]` + `[scope]` passthrough + `[[rule]]`); the DB is canonical and
`body_toml` preserves the `[scope]` block across round-trips.

## Plan → task ingestion

`src/tracker/ingest.rs` (line-oriented markdown parser): `# H1`→`plan`,
`## H2`→`epic`, `### H3`→`task`, deeper→`sub_task`, `- [ ]/[x]`→`todo`
(`[x]` seeds `claimed_done`, **not** verified). Idempotent re-ingest via a stable
`public_id`. Exposed as `work_item_ingest_plan` (MCP) + `POST
/api/tracker/ingest_plan` (REST, mirrors `session_observe`) + the PostToolUse:
ExitPlanMode hook `~/.claude/hooks/tracker-ingest-plan.sh`. The credential-gated
`POST /api/tracker/record_evidence` lets hooks/CI post **trusted-source**
evidence — closing the verify loop the deferred Stop-hook harness plugs into
with zero schema churn (`acceptance_criteria.gate` + `verification_evidence.
detail_json` reserve α/β/γ).

## A2A collaboration & visibility

- **Identity:** canonical free-text `agent_id` (lowercased `clientInfo.name` via
  `extract_caller`); `agent_identity` view reconciles to `a2a_agents`
  advisorily. No write path blocks on registration.
- **Atomic claiming:** single-statement CAS (`work_item_claim`); `FOR UPDATE SKIP
  LOCKED` fan-out (`work_item_claim_next`); owner-gated `release`/`handoff`; each
  writes a `work_item_claims` row.
- **Crash-safety:** `lease_expires_at` makes a dead agent's claims stealable; the
  `work-item-presence` cron (`src/cron/work_item_presence.rs`, light/
  unconditional, registered in `scheduler.rs`) NULLs expired leases (+ `expire`
  ledger row) and decays `agent_presence` active→idle→offline. `agent_heartbeat`
  renews leases.
- **Presence is activity-driven:** `touch_presence` is called from every
  claim/claim_next/release/handoff/progress/heartbeat write, so the roster is
  never stale between heartbeats.
- **Visibility:** `work_item_who_owns`, `agent_activity` (scoped or roster),
  `work_item_activity` (union of progress + claim events, agent-attributed).
  Progress rows are attributed via `actor_id` (the agent id) while provenance
  stays `agent_write` — attribution and trust are orthogonal.

## Experiment integration (the elegant win)

A `kind='experiment'` work_item is a lightweight tracking handle; the rich
hypotheses/runs/samples/results stay in the experiment tables, linked via the
`work_item_experiment` bridge. The experiment subsystem is *already* a
verification engine: a **frozen, pre-registered** acceptance criterion decided by
the statistical engine over raw samples — exactly the tracker's "machine-
checkable + un-fakeable" model. So when `experiment_decide` renders a verdict it
auto-posts `verification_evidence{source='experiment', runner_identity=
'pgmcp-stats-engine', ...}`. Because `source='experiment'` is in the trusted set,
an *accepted* verdict legitimately flips the linked task to `verified` through
the normal gatekeeper path (`work_item_link_experiment` seeds the
`experiment_verdict` criterion). A `rejected`/`inconclusive` verdict records
`fail`/`unknown` — the investigation is *concluded* but not *verified*.

## MCP tool surface

~46 tools under `src/mcp/tools/work_items/` (crud, lifecycle, tags, progress,
analysis, definitions, verify, ingestion, collab, visibility, relations,
reporting, experiment_link). Each = a `<Name>Params` struct + a `#[tool]` method
forwarding via `instrumented_tool_wrap` + a `dispatch_tool!` arm in
`call_tool_cli` + a smoke test (enforced by `query_inventory_vs_coverage`).

## Consequences

- The tracker is the DB **system-of-record** the deferred serene-eclipse Stop-
  hook harness plugs into as an *evidence producer* (zero schema churn).
- CUDA stays mandatory; no cargo features; migrations stay idempotent; the
  trust boundary is structural and property-tested.
- Embedding-on-write degrades to NULL on transient GPU OOM (non-fatal); the
  embedding-migration cron backfills.

## Alternatives rejected

- **Bi-temporal `valid_from/valid_to` on items** — a work item is a mutable
  tracked object, not an asserted fact; the append-only progress + status-history
  + evidence logs already give the audit timeline.
- **Trusting an agent-posted `verified`** — the entire point; rejected by
  construction (no Agent arm into `verified`).
- **LISTEN/NOTIFY live feed** — deferred; the 200 ms SSE-poll idiom suffices. The
  ledger writers reserve the `pg_notify` seam.
