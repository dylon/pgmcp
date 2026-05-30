# pgmcp — Codex Working Rules

## Verification

Before declaring code changes complete, run:

```bash
./scripts/verify.sh
```

Focused `cargo test` or `cargo clippy` runs are useful during iteration, but
they do not replace the full verification gate.

## Project Notes

- CUDA is mandatory; there is no CPU-only cargo feature.
- The main binary and library expose the same module tree. Add new top-level
  modules to both `src/main.rs` and `src/lib.rs` when applicable.
- `pgmcp` serves one shared MCP index. Claude Code and Codex CLI can both query
  synthetic agent projects such as `claude` and `codex` when connected to the
  same daemon.
- Keep transcript parsers conservative: index useful user/assistant/tool text,
  and skip credentials, encrypted payloads, reasoning internals, cache/state,
  and oversized tool output.

## Work-item / bug tracking

`src/tracker/` + `src/db/queries/work_items.rs` + `src/mcp/tools/work_items/`
implement a hierarchical work-item tracker (15 kinds) with an evidence-gated
trust boundary: an agent can self-report `claimed_done` but **cannot
self-verify, self-defer, or self-confirm** — those transitions have no `Agent`
arm in `src/tracker/transition.rs`. Full design:
`docs/decisions/004-work-item-tracker.md`.

Bugs are first-class. Create with `kind='bug'` — it is born in `triage` and
carries a `severity` (`critical | high | medium | low`) plus structured
reproduction / expected-vs-actual / environment fields. A human confirms a bug
with the user-token `work_item_triage` (`triage → confirmed`; requires a severity
and reproduction to be present); `work_item_resolve` closes one *without* a fix
(`→ cancelled`) with a categorized `resolution` (`wont_fix | duplicate |
cannot_reproduce | by_design`, `duplicate` recording a `duplicates` relation). A
*fix* is verified through the normal gatekeeper+evidence path (`fixed` is not
settable by hand). `work_item_triage` / `work_item_resolve` / `work_item_defer`
require `[tracker] user_token`; agents do not have it.

**Ergonomics (v16).** `work_item_view` runs a fixed smart-view (`my-work` /
`needs-triage` / `overdue` / `blocked` / `next-actionable`);
`work_item_next_actionable` returns the single best workable-now item;
`work_item_assign` sets the **durable** `assignee` (distinct from the ephemeral
`claimed_by` lease — owns vs. actively-executing; never auto-cleared);
`work_item_history` is one item's full timeline; `work_item_bulk` applies a
`BulkOp` (`set_status`/`tag`/`untag`/`reprioritize`/`assign`) over a resolved set
through the per-item chokepoint. **Auto-unblock:** verifying an item moves
dependents that were `blocked` solely on it `blocked → ready` as `Actor::System`
in-tx (System has no judgment-state arm, so it can unblock but never complete).

**Git/PR close-the-loop (v17).** Reference an item from a commit/PR with
`#<public_id>` (a touch → `in_progress`) or `fixes|closes|resolves|implements|refs
<public_id>` (a close → `claimed_done`). The git indexer auto-links + auto-transitions
referenced items (per-project `[git] auto_link_items`, default on with
`index_history`); `work_item_link_commit` is the manual link. **Trust boundary:** a
commit/merge is *agent-grade* (`Actor::Agent`) and can NEVER reach `verified` — the
git→`verifying` candidate and a green build are different things. Only a CI-posted
`source='ci'` evidence row (`POST /api/tracker/ci_evidence`) flips an item to
`verified` through the gatekeeper; `POST /api/tracker/pr_event` only stages a merge
as a `verifying` candidate. The `findings-promotion` cron can materialize
high-confidence findings into `pending` items (opt-in: `[tracker]
auto_promote_findings`, default OFF).

**Proactive digest.** Off by default; set `[digest] enabled = true` to surface
tracker/health/trend state in the SessionStart `pgmcp context` and the
UserPromptSubmit `additional_context`. It is read-only (a source-grep test bans
transition symbols from `src/digest/`). Trends/forecasts come from
`quality_trend`/`quality_forecast` (and `work_item_burndown`'s `slope_per_day` /
`regression_eta_days`), fed by the `quality-history` cron.

## Session-level mandates

`src/sessions.rs` extracts imperative directives from user prompts via a
tiered regex pipeline and persists them by `session_id` in 12 polarities
(always / never / prefer / avoid / remember / from_now_on / correction /
permission / constraint / mandate / process_rule / project_rule).
The UserPromptSubmit hook `~/.claude/hooks/pgmcp-rag.sh` POSTs each
prompt to `POST /api/session/observe`, which extracts, persists, and
returns a combined `additional_context` Markdown block (active mandates
+ RAG hits). MCP tools `session_mandates` and `promote_session_mandate`
let the agent introspect and elevate session-scoped rules to durable
project/workspace scope.

## Software pattern catalog

The curated catalog lives at `src/patterns/` — 21 per-family files:
`gof.rs`, `solid_grasp.rs`, `principles.rs`, `functional.rs`,
`concurrency.rs`, `architecture.rs`, `declarative.rs`, `anti_patterns.rs`,
`code_smells.rs`, `security.rs`, `testing.rs`, `idioms.rs`, `aop.rs`,
`observability.rs`, `deployment.rs`, `data_engineering.rs`,
`api_design.rs`, `ml_ai.rs`, `distributed_data.rs`, `kubernetes.rs`,
`sources.rs`. `mod.rs` declares the `ParadigmSeed`/`PatternSeed`/
`SourceDescriptor` types, the `const fn pat(...)` helper, the assembler
`pattern_seeds()`, and unit tests that enforce slug/paradigm/kind
referential integrity. Add new patterns by appending entries to the
appropriate per-family file; the assembler and tests pick them up
automatically.

`kind` is constrained by `software_patterns_kind_check` to
`pattern | anti_pattern | principle | code_smell` (see
`src/db/migrations.rs`). When seeding a *principle* (SOLID, GRASP, DRY,
KISS, …) or *code smell* (Long Method, Feature Envy, …), use those kinds
rather than re-purposing `pattern` / `anti_pattern`.

The 14 paradigms cover: procedural, object-oriented, functional, logic,
event-driven, concurrent, parallel, aspect-oriented, distributed-systems,
reactive, dataflow, declarative, actor-model, and machine-learning
engineering. The embedding signature is `pgmcp-pattern-embedding-v3` —
bump it (in `src/mcp/tools/tool_software_patterns.rs`) whenever seed
prose changes so existing installs re-embed cleanly.
