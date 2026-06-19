# ADR-022: Behavioral-mandate enforcement architecture

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-016 (adaptive tool surface), ADR-021 (logging convention),
  ADR-004 (work-item tracker trust boundary). Mirrors the cross-agent seed pattern of
  `src/docguidelines/` (the documentation-guidelines enforcement surface).

## Context

The user maintains a set of standing behavioral mandates for how agents should work. Four
are in scope here:

1. **Full generality (no overfitting).** "Never overfit a solution to solve one problem
   just to regress elsewhere; all solutions must be fully generalized."
2. **Boy Scout rule (fix every bug).** "Always follow the boyscout rule: leave a system in
   better shape than it was when you started… Fix all the issues you discover whether they
   were pre-existing or not. No bug, regardless its rarity, is acceptable."
3. **Capture command output, then clean up.** "Always pipe command output for validation,
   compilation, and evaluation tasks to files for follow-up analysis… Then… clean up all
   temporary files."
4. **Occam's Razor (simplest, not simpler).** "Adhere to Occam's Razor such that changes
   are kept as simple as possible to accomplish their goals but no simpler… you will not
   make extraneous changes."

Before this ADR the *only* surface for these was re-injection: pgmcp's session-mandate
pipeline (`src/sessions.rs`) re-injects extracted prompt mandates as
`additionalContext`, and the user's `CLAUDE.md` files are read at SessionStart. Both are
Claude-only and *model-discretionary* — there was no always-on, cross-agent surface and
no mechanical gate for any of the four.

## Decision

Enforce each mandate at the strongest layer its nature admits — a matrix, not a single
mechanism:

| Mandate | Mechanical oracle? | Primary enforcement | Secondary |
|---|---|---|---|
| (3) pipe-output + cleanup | **yes** (inspect the command string; scan for leftover files) | **PreToolUse:Bash hook** + Stop/SessionEnd temp-sweep (user-scope) | re-injection |
| (2) boyscout / fix-all-bugs | **partial** (the *tracked* `kind='bug'` subset anchored to touched files) | **`pgmcp bug-gate`** — verify.sh Gate 9 (repo) | re-injection |
| (1) full generality; (4) Occam's Razor | **no** (judgment properties) | **durable cross-agent re-injection** (`src/engprinciples/`) | optional review |

### 1. Cross-agent re-injection — `src/engprinciples/` (REPO)

A new DB-free seed module, a direct sibling of `src/docguidelines/`, holds the four
mandates **verbatim** (the wording is the contract). It is the only *universal, always-on*
surface pgmcp controls — injected into every MCP client's `instructions` on the
`initialize` handshake (`compose_instructions`, so Codex/Cursor/etc. with no prompt hook
also receive it), and surfaced in `orient`, the `engineering_principles` MCP tool, and the
`pgmcp://engineering-principles` resource. This is the *primary* mechanism for the two
judgment mandates (1, 4), which have no mechanical oracle, and a *belt-and-suspenders*
reinforcement for (2, 3). The project `CLAUDE.md` also mirrors the four (the high-signal
Claude channel).

### 2. Boyscout bug-gate — `pgmcp bug-gate` + verify.sh Gate 9 (REPO)

A new `open_bugs_anchored_to_paths` query (`src/db/queries/work_items.rs`) finds open
(`status NOT IN ('verified','cancelled','deferred')`) `kind='bug'` work-items anchored via
`work_item_code_anchor` to files touched by the current git diff (suffix-matched like
`resolve_file_id_by_path`). A new `pgmcp bug-gate` CLI subcommand
(`src/cli/bug_gate.rs`) reports them and exits non-zero (blocking) unless `--warn-only`.
verify.sh runs it as **Gate 9** (blocking). It **self-skips loudly** (exit 0, logged +
printed) outside a git work tree or when the DB is unavailable, so it never silently
disables itself nor blocks a contributor lacking the indexed workspace. Honest scope: the
gate covers the *tracked, anchored* bug subset; "no bug however rare" is a judgment the
gate cannot fully mechanize — re-injection carries the spirit, the gate enforces the
tracked subset.

### 3. Output-capture + cleanup hooks (USER-SCOPE, `~/.claude/`)

Two PreToolUse/Stop hooks (outside the repo; the user installs them and wires
`settings.json`):

- `~/.claude/hooks/pgmcp-output-capture-enforce.sh` (modeled on `git-guard.sh` +
  `pgmcp-grep-enforce.sh`): on a Bash tool call, if the command is a heavy
  validation/compile/eval command (`cargo build|test|nextest|clippy|smoke|verify-*`,
  `verify.sh`, `make`, `coqc`, `tlc`, `tlapm`, `pytest`, large `*bench*`) **and** lacks a
  file sink (`| tee <path>`, `>`, `>>`, `&>`), it emits a `permissionDecision: "ask"`
  steering the agent to `… 2>&1 | tee /tmp/pgmcp-$SESSION_ID-<task>.log`. Gated behind
  `PGMCP_HOOK_MODE=enforce` (off by default, like the grep/glob-enforce hooks); deduped;
  pure-local (no daemon dependency); always `exit 0`.
- `~/.claude/hooks/pgmcp-temp-sweep.sh` (Stop/SessionEnd): lists leftover
  `/tmp/pgmcp-$SESSION_ID-*.log` capture files as `additionalContext` so the agent deletes
  them; warn-only by default (opt-in `PGMCP_TEMP_SWEEP_MODE=delete`).
- A small `pgmcp_emit_ask` helper is added to `~/.claude/hooks/lib/pgmcp-common.sh`
  (only `pgmcp_emit_deny` existed).

## Enforcement ceiling (honest)

MCP `instructions`/`orient`/tool/resource surfaces are **advisory** — they maximize reach
(every agent always sees the mandates), not compulsion; pgmcp never observes an agent's
edits, so it cannot hard-gate judgment quality. The only *compulsory* layers are the
user-scope PreToolUse hook (harness-enforced `permissionDecision`) and the verify.sh
bug-gate (CI/pre-push). Per the user's own memory (`feedback_hook_reliability_layers`),
`additionalContext` is model-discretionary; this ADR therefore spends the *enforced*
mechanisms on the two mechanizable mandates and accepts re-injection as the realistic
ceiling for the two judgment mandates.

## Repo vs user-scope

- **Repo (PR + verify.sh):** `src/engprinciples/`, its wiring in `src/mcp/server.rs`
  (`compose_instructions` banner, tool registration, resource), `src/mcp/tools/
  tool_engineering_principles.rs`, the `engineering_principles` key in
  `src/mcp/tools/tool_orient.rs`, `open_bugs_anchored_to_paths`, `src/cli/bug_gate.rs` +
  the `Commands::BugGate` wiring, verify.sh Gate 9, and the `CLAUDE.md` mirror.
- **User-scope (`~/.claude/`, applied by the user — not in any PR):** the two Bash hooks,
  the `pgmcp_emit_ask` helper, and the `settings.json` PreToolUse:Bash + Stop wiring.

## Consequences

- **Positive:** the four mandates reach *every* agent always-on (not just Claude, not just
  discretionarily); two gain real mechanical gates; the seed is a single verbatim source
  of truth with referential-integrity tests.
- **Negative:** Gate 9 can block a push when a pre-existing anchored bug sits in a touched
  file — which is the boyscout rule operating as intended, but may surprise. `--warn-only`
  is the documented escape during transitions.
- **Neutral:** the hooks are user-scope; activating them is a one-time `settings.json`
  edit the user controls (they affect all the user's projects, not just pgmcp).
