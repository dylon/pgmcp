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
tasks/todos/fixmes/bugs/ideas/brainstorms/notes/questions/action-items/goals/
epics/plans/sub-tasks/nice-to-haves/experiments (15 kinds) in an **arbitrary-depth
tree**, validate
plans against reusable definitions, gate "done → verified" on machine-checkable
evidence an agent **cannot fabricate**, auto-ingest agent plans into the tree,
and coordinate multiple agents over a shared plan with live presence + an
activity feed.

## The load-bearing decision: trust boundary lives in the state machine

An agent's word is not trusted. This is enforced *structurally*, not by
convention, in `src/tracker/transition.rs`:

- Status lifecycle (12 states): `pending → ready → in_progress → claimed_done →
  verifying → verified`, plus `blocked`, `rejected`, `deferred`, `cancelled`, and
  the bug-triage states `triage → confirmed` (a reported bug awaits a user-token
  confirmation before it is actionable; see "Bug tracking" below).
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
  4. **`→confirmed` (bug triage) is `User`-only** — an agent may *report* a bug
     (`→triage`) and propose a severity, but confirming it real is a human
     judgment (token-gated `work_item_triage`, the same authority mechanism as
     `defer`); there is no `Agent` arm into `confirmed`.

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
- **`v12_bug_tracker`** (`BUG_TRACKER_V1 = 12`) — first-class bug tracking: a
  nullable `severity` column on the `work_items` spine and a 1:1
  `work_item_bug_details` sidecar (reproduction / expected-vs-actual /
  environment / affected & fixed version / root cause / regression flag / triage
  attribution / resolution). The severity CHECK is reconciled into
  `install_work_items_checks` (column-guarded, since the every-boot reconcile
  runs before this migration on a fresh install) so a future severity-vocabulary
  edit propagates like kind/status; the `idx_work_items_active` partial index is
  dropped + recreated widened to include `triage`/`confirmed`.

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

## Bug tracking (v12)

A `kind='bug'` is the first-class defect type, distinct from `fixme` by
*provenance*: `fixme` is a code-anchored marker auto-promoted from a
`FIXME:`/`TODO:` comment, whereas a `bug` is an **observed** defect with a
reporter, a severity, and a reproduction. A bug carries two orthogonal axes —
**`severity`** (impact: `critical | high | medium | low`, the closed `Severity`
enum in `src/tracker/severity.rs`) and the existing **`priority`** (urgency);
setting a severity with no explicit priority seeds a default urgency (and never
clobbers an explicit one).

Lifecycle: a bug is born in **`triage`**. The user-token **`work_item_triage`**
tool confirms it (`triage → confirmed`), **requiring** a severity and
reproduction to be present — then it flows through the normal `in_progress →
claimed_done → verifying → verified` path (a fix is verified by the same
un-fakeable gatekeeper+evidence gate as any other work). Closing a bug *without*
a fix is the user-token **`work_item_resolve`** tool: it records a categorized
**`resolution`** (`wont_fix | duplicate | cannot_reproduce | by_design`, the
closed `BugResolution` enum; `duplicate` also records a `duplicates` relation) and
transitions `→ cancelled` via the same `scope_negotiations` path as `defer`.
`fixed` is *not* settable via `resolve` — it is reached only through the verified
path. `triage`/`confirmed` are *open* states (they dilute a parent's verified
fraction like `pending`); only `deferred`/`cancelled` are roll-up-excluded.
Embed-on-write and the cron drain fold the bug-detail text (reproduction /
expected-vs-actual / root cause) so "find similar bugs" semantic search sees it,
and structured bug fields are checkable by `required_field` definition rules
(`applies_to_kind='bug'`).

## MCP tool surface

~55 tools under `src/mcp/tools/work_items/` (crud, lifecycle, tags, progress,
analysis, definitions, verify, bugs, ingestion, collab, visibility, relations,
reporting, experiment_link, views, bulk, git_link — the last three added by the
zazzy-galaxy roadmap below). Each = a `<Name>Params` struct + a `#[tool]` method
forwarding via `instrumented_tool_wrap` + a `dispatch_tool!` arm in
`call_tool_cli` + a smoke test (enforced by `query_inventory_vs_coverage`).

## Consequences

- The tracker is the DB **system-of-record** the deferred serene-eclipse Stop-
  hook harness plugs into as an *evidence producer* (zero schema churn).
- CUDA stays mandatory; no cargo features; migrations stay idempotent; the
  trust boundary is structural and property-tested.
- Embedding-on-write degrades to NULL on transient GPU OOM (non-fatal); the
  embedding-migration cron now backfills `work_items` too (a previously-missing
  drain — every tracker semantic search filters `WHERE embedding IS NOT NULL`, so
  a failed write had silently dropped the item from search), folding the
  bug-detail sidecar text to match the bug-aware embed-on-write recipe.

## Alternatives rejected

- **Bi-temporal `valid_from/valid_to` on items** — a work item is a mutable
  tracked object, not an asserted fact; the append-only progress + status-history
  + evidence logs already give the audit timeline.
- **Trusting an agent-posted `verified`** — the entire point; rejected by
  construction (no Agent arm into `verified`).
- **LISTEN/NOTIFY live feed** — deferred; the 200 ms SSE-poll idiom suffices. The
  ledger writers reserve the `pg_notify` seam (Phase 4 *wires* the seam for the
  digest — `pg_notify('pgmcp_digest', …)` — but leaves it off by default, with
  still no built consumer; see the Phase-4 addendum).

---

# Addendum: the zazzy-galaxy roadmap (snapshots → trajectories → push)

- **Status:** Accepted (implemented 2026-05-29, `feat/work-item-tracker`)
- **Design plan:** `~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`
- **Theme:** the base tracker recorded *state* and the quality subsystem reported
  *snapshots*; this four-phase roadmap turns both into *trajectories* (Phase 1),
  sharpens the day-to-day ergonomics over that state (Phase 2), closes the loop
  to real repo activity without ever weakening the trust boundary (Phase 3), and
  finally *pushes* the standing state to agents instead of waiting to be polled
  (Phase 4). The load-bearing invariant from the base ADR — **the trust boundary
  lives in the state machine; an agent's word is never trusted** — is preserved
  verbatim and, where new write paths appear (commit/PR auto-linkage, CI
  evidence, findings promotion), re-proven against the same matrix.

## Phase 1 — quality trends & forecasting

The v9 `quality_report_history` table had existed since the quality subsystem
shipped but **nothing ever populated it** — every quality metric was a single
snapshot with no history to regress against. Phase 1 closes that gap and turns
the snapshot into a trajectory.

- **`quality-history` cron** (`src/cron/quality_history.rs`, registered in
  `scheduler.rs`): snapshots every indexed project's pillar GPAs into
  `quality_report_history` each tick. Heavy (it fans the quality collectors out
  via `crate::quality::aggregate`), so it runs behind the heavy-cron gate;
  interval-gated on `[cron] quality_history_interval_secs > 0` (default 6 h /
  21600 s; 0 disables). Best-effort per project (one project's aggregation
  failure never aborts the sweep); persists only the pillar GPAs (findings/fixes
  are recomputed on demand, not stored).
- **`src/quality/forecast.rs`** — three pure, DB-free, unit-tested functions over
  a plain `f64` series so both the tools and the digest share one trajectory
  math: `ols_slope(&[(x, y)])` (ordinary-least-squares slope, *units of y per
  unit of x* — callers pass `x` in days, so "per day"; `None` for <2 points or a
  degenerate all-equal-`x` fit); `weeks_to_threshold(latest, slope_per_day,
  threshold)` (weeks until a moving metric crosses a red line; `None` when flat,
  diverging, or already past); `pct_change(prev, latest)` (signed percent change,
  `None` off a zero baseline).
- **New MCP tools** `quality_trend` (the realized slope + percent-change over a
  window) and `quality_forecast` (the projected red-line crossing). Both pull
  their series from `quality_report_history` via `crate::quality::history`.
- **`work_item_burndown` gained `slope_per_day` + `regression_eta_days`** — the
  same trajectory treatment applied to the tracker's own roll-up: not just "this
  plan is 40 % verified" but "verified fraction is climbing/sliding X/day and
  (re)crosses its target in Y days".

The "trajectory" framing is the unifying idea: a snapshot says *engineering GPA
is 2.4*; the trajectory layer says *GPA is falling 0.1/week and hits the C-grade
boundary (2.0) in ~4 weeks*.

## Phase 2 — tracker ergonomics & next-action

Migration **v16** (`work_item_assignee_v1`, `src/db/migrations/v16_assignee.rs`)
adds a *durable ownership* axis to the `work_items` spine, deliberately distinct
from the v5 *ephemeral lease*:

- `assignee` / `assigned_at` / `assigned_by` — nullable free-text (an agent or
  human id, exactly like `claimed_by`; no CHECK). `assignee` answers "who
  **owns** this work?" and is set via `work_item_assign` and **never
  auto-cleared**.
- This is orthogonal to `claimed_by` (v5), the CAS lease that auto-expires
  (crash-safety) and is cleared on release/handoff/expiry — "who is **actively
  executing** this right now?". A partial index `idx_work_items_assignee
  (assignee, priority DESC) WHERE assignee IS NOT NULL` serves the `my-work`
  queue and costs nothing for the common unassigned rows. `ADD COLUMN IF NOT
  EXISTS` with no default = an instant metadata-only change (no rewrite).

Two closed request-shaping enums in `src/tracker/views.rs` (ADR-003 closed-enum
idiom — `ALL` + `as_str` + `parse` + a golden test — but **no DB CHECK**, since
these are params on the wire, not stored columns):

- `SmartView` (kebab-case): `my-work` / `needs-triage` / `overdue` / `blocked` /
  `next-actionable` — five fixed built-in queues over the existing
  `list_work_items` path. A new view is a code change, like a new status; this is
  *not* a persisted `saved_views` table.
- `BulkOp` (snake_case): `set_status` / `tag` / `untag` / `reprioritize` /
  `assign` — the per-item action a `work_item_bulk` call applies.

New tools: `work_item_view` (run a `SmartView`), `work_item_next_actionable` (the
single best workable-now item), `work_item_assign` (set/clear the durable
assignee), `work_item_history` (the append-only status/progress/claim/evidence
timeline for one item), `work_item_bulk` (apply a `BulkOp` to a resolved set).

### Auto-unblock cascade (+ the System-actor safety argument)

When an item reaches `verified`, dependents that were `blocked` **solely** on it
auto-advance `blocked → ready` — in the **same transaction**, as
`Actor::System`, routed through the same `check_transition` chokepoint. The
mechanics (`src/db/queries/work_items.rs`): after the verifying `UPDATE`, select
every `blocked` dependent linked by a `depends_on`/`blocks` relation to the
just-verified item, and for each, re-evaluate whether an *unresolved* blocker
remains (a blocker counts as cleared once it is in
`verified`/`claimed_done`/`deferred`/`cancelled`, evaluated within the tx so the
just-verified row counts); if none remains, transition it `blocked → ready` and
write the `actor_kind='system'` history row (`auto-unblocked: last blocker
verified`). `work_item_bulk`'s `set_status` loops through the *same* per-item
chokepoint, so a bulk verify fires the cascade per item.

**Why this is safe (and not a trust-boundary hole):** `Actor::System` is a
server-only actor with **no arm into any judgment state** — the matrix has no
`System → verified`/`rejected`/`deferred`/`confirmed`. `System`'s *only* legal
move here is `Blocked → Ready` (asserted by
`transition.rs::system_absent_from_judgment_columns`), and the cascade routes
even that move through `check_transition`. So the cascade can *unblock* work but
can never *complete* it: a human/CI still verifies the dependent through the
normal gate. This is the same structural argument as the base ADR, extended to a
fourth actor.

## Phase 3 — git / PR close-the-loop (without weakening trust)

The base tracker could record work but had no idea when a *commit* or a *PR*
acted on it. Phase 3 wires that in — and is the most trust-sensitive surface in
the roadmap, so the invariant is stated bluntly:

> **THE TRUST BOUNDARY.** A commit or a merge is an **agent-grade** signal: the
> indexer / `pr_event` handler runs the resulting transition as `Actor::Agent`,
> which has **no arm into `verified`** anywhere in the matrix. A commit/merge can
> therefore advance an item *toward* done — at most to `claimed_done` (a claim)
> or stage it as a `verifying` candidate — but can **never reach `verified`**.
> The only path to `verified` remains a passing CI-posted `source='ci'` evidence
> row through the existing gatekeeper transition. Code is cheap to write and easy
> to mislabel; a green build from an external runner is the thing that is hard to
> fake, and it stays the sole verifier.

Migration **v17** (`git_links_v1`, `src/db/migrations/v17_git_links.rs`) — two
additive tables, neither touching the `work_items` spine:

- `work_item_git_links` — the item ↔ commit/PR/branch join.
  `UNIQUE(item_id, link_type, ref_value)` is the idempotency key (re-scanning a
  commit or re-running `work_item_link_commit` upserts, never duplicates).
  `commit_id` is an optional FK into `git_commits` (`ON DELETE SET NULL` — a
  re-indexed/pruned commit leaves the link intact, just unresolved);
  `detected_by` is a closed two-value provenance (`manual` | `auto_scan`).
  `link_type` is CHECK-constrained from `GitLinkType`.
- `work_item_finding_provenance` — the idempotency ledger for cron promotion.
  `provenance_key` is `UNIQUE`, so promoting the same finding twice is a no-op;
  `finding_source` is CHECK-constrained from `FindingSource`.

Closed enums in `src/tracker/git_link.rs` (ADR-003 idiom + `sql_in_list()` +
golden test): `GitLinkType` = `commit` | `pr` | `branch` (with an
`infer_from_ref` shape heuristic for the `work_item_link_commit` ergonomic);
`FindingSource` = `bug_prediction` | `documented_tech_debt` (each mapping to the
`work_items.kind` it materializes via `item_kind()` — `bug` and `fixme`
respectively).

**The `fixes #<public_id>` convention** (`src/tracker/commit_ref.rs`): a work
item's `public_id` is a kebab slug plus a short hex suffix
(`my-task-3f9a1c`). Two reference forms are parsed out of a commit message (or PR
title+body):

- a **bare hash mention** `#my-task-3f9a1c` — a *touch*: links the commit and
  (agent-grade) advances a not-yet-started item to `in_progress`;
- a **closing verb** `fixes|fixed|fix|closes|closed|close|resolves|resolved|
  resolve|implements|implemented|implement|refs|ref` followed by `#?<public_id>`
  — a *closing touch*: additionally promotes an `in_progress` item to
  `claimed_done` (a claim, **not** a verification). The bare-id form must be
  hyphenated so `fixes the bug` does not capture `the`; the `#`-prefixed form
  accepts any token.

The **agent-grade auto-transition policy** (`src/tracker/auto_transition.rs`,
`next_auto_status(from, is_closing)`) maps the parsed reference to a status the
`Actor::Agent` path may legally reach: not-yet-started (`pending`/`confirmed`/
`ready`/`blocked`) → `in_progress` on any reference; `in_progress` → `claimed_done`
only on a closing verb; everything else (already claimed/verifying/terminal, and
the user-only-exit `triage`) is a no-op. The exhaustive
`never_returns_a_judgment_status` test asserts, over `WorkItemStatus::ALL ×
{closing, non-closing}`, that the function is *structurally incapable* of
returning a judgment status and that every move it does propose is `Agent`-legal
— and the caller still runs the result through `check_transition`, so even a
policy bug only ever narrows what the agent path may attempt.

- Tool `work_item_link_commit` — a hand-made link (inferring `link_type` from the
  ref shape when omitted).
- The **git indexer** auto-links every indexed commit whose message references an
  item and runs the agent-grade auto-transition — gated per-project by
  `[git] auto_link_items` (default on when `index_history` is on).
- **REST** `POST /api/tracker/ci_evidence` — the *trusted* loop: CI posts a
  `source='ci'` evidence row by `public_id`, which (being in the trusted-source
  set) flips a `claimed_done`/`verifying` item to `verified` through the existing
  gatekeeper transition. This is the **only** way Phase 3 reaches `verified`.
  `POST /api/tracker/pr_event` — a PR open/merge links the branch/PR and stages a
  merge as a `verifying` *candidate* (`Actor::Agent`), never `verified`.
- **`findings-promotion` cron** (`src/cron/findings_promotion.rs`): idempotently
  materializes high-confidence `bug_prediction` files (score ≥ the per-project
  threshold) into `pending` `bug` items and high-severity `documented_tech_debt`
  markers (FIXME/BUG/HACK/…) into `pending` `fixme` items, so a finding surfaces
  in the tracker/digest instead of dying as JSON behind a tool call. Promotion
  goes through `promote_finding` keyed on the stable `provenance_key` (a re-run
  is a no-op), caps promotions per (project, source) per run, and lands items in
  `pending` — **never** pre-`confirmed` (confirmation is user-only). Per-project
  opt-in, default **OFF** (`[tracker] auto_promote_findings = true`); the cron
  skips every project that has not opted in. Globally interval-gated by
  `[cron] findings_promotion_interval_secs` (default 6 h; 0 disables).

## Phase 4 — the proactive digest

pgmcp computes a great deal it never proactively shows — overdue tracker items, a
growing embedding backlog, a falling engineering GPA — all of which dies as JSON
unless an agent polls for it. Phase 4 turns pull into push by riding the two
channels agents **already** read, with no new endpoint and no new hook.

Migration **v18** (`digest_emissions_v1`,
`src/db/migrations/v18_digest_emissions.rs`) adds `digest_emissions`, a
rate-limit ledger mirroring `v11_nudge_emissions`: `(session_id, channel,
project_id, content_sha256, item_count, ts)`. `channel` is CHECK-constrained from
the closed `DigestChannel` enum. Local-only, same privacy posture as
`nudge_emissions` — it stores the digest's **sha256 fingerprint + item count
only**, never the body or any prompt text.

The `src/digest/` subsystem (`compose_digest`) assembles up to three sections:

| Section     | Source                                                                                      | Severity signal                                                  |
|·············|·····························································································|·················································................|
| **TRACKER** | overdue / blocked / needs-triage / next-actionable via the Phase-2 `list_work_items` filters | overdue → High; blocked/triage → Notice; actionable → Info       |
| **HEALTH**  | index staleness (`projects.last_scanned_at`), embedding backlog, recently-panicked crons     | backlog/staleness → Notice/High; a panicked cron → Critical      |
| **TREND**   | Phase-1 GPA slope + forecast (`quality::history` + `quality::forecast`), gated on `include_trends` | approaching the C-grade red line → High/Notice               |

`compose_digest` takes `Option<&StatsTracker>`: the daemon (REST) passes `Some`
(so HEALTH can include the cron-failure signal); the CLI (`pgmcp context`, no
live stats) passes `None`. It is best-effort — any single source that errors (a
missing table on a partial install) is skipped rather than failing the whole
digest. The rendered block is severity-sorted and byte-budgeted (`max_bytes`):
the most urgent line always survives truncation. Geometric glyphs (`·`/`▸`/`▲`/`◆`),
not emoji.

**The channels it rides** (closed `DigestChannel` enum): `session_start` — the
**SessionStart** `pgmcp context` CLI output; `prompt` — the **UserPromptSubmit**
`/api/session/observe` `additional_context`; `webhook` — an optional outbound POST
(daemon-only, fire-and-forget, min-severity-gated, empty-URL default off).
`maybe_emit` dedupes by `content_sha256` within `ttl_secs` and rate-limits
per-session (`max_per_session`) using the nudge-gate idiom, then records the
emission.

**The read-only guarantee (the unifying invariant).** The digest is
*structurally* read-only: it issues only `SELECT`s for everything it surfaces,
plus **exactly one write** — the INSERT into its own `digest_emissions`
rate-limit ledger. It performs **no status transitions** and constructs no
`Actor`. `pgmcp-testing/tests/digest_trust_boundary.rs` is a source-grep test
that bans `set_work_item_status` and `Actor::` from every file under
`src/digest/`, so this property cannot silently regress — the same
structural-not-conventional discipline as the base ADR's trust boundary.

**The `pg_notify` seam.** `notify_digest_ready` emits
`pg_notify('pgmcp_digest', payload)` (a short JSON summary — session, channel,
max-severity, item count, sha — **never** the digest body) on the daemon path
when `[digest] pg_notify = true` (default false). This *wires* the seam the base
ADR's "LISTEN/NOTIFY live feed" alternative reserved, but builds **no consumer**
— there is none in the single-user setup; a future SSE bridge would
`LISTEN pgmcp_digest`. The whole `[digest]` section is `enabled = false` by
default, so a stock install stays inert.
