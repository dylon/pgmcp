# pgmcp — Agent Working Rules

## Non-negotiable: verify before claiming work complete

Before declaring any code change complete, run the full verification gate:

    ./scripts/verify.sh

If any step fails, the work is not done. There are no environment-variable
overrides or opt-outs. The script is the contract.

**Run `verify.sh` ONLY after every other task in the request is complete — never
mid-task, never to check progress.** It is the single final gate, run exactly
once at the end (~10 min: full `build --all-targets` + clippy + release tests +
GPU smoke). During the interim, iterate with `cargo nextest run --release --bin
pgmcp <filters>` and targeted `cargo clippy --bin pgmcp --all-targets`, and grow
an exhaustive test suite as you go so the final `verify.sh` is a formality, not a
discovery step. (This timing rule governs *when* to run the contract; it is not
an opt-out — the contract itself still has none.)

`./scripts/verify.sh` is also enforced on every `git push` via the pre-push
hook at `.githooks/pre-push`. Activate it once per clone:

    git config core.hooksPath .githooks

Bypass (`git push --no-verify`) is reserved for genuine emergencies; do not
automate it.

## Quick wrappers during iteration

These are individual gates, NOT a replacement for `scripts/verify.sh`:

    cargo verify-build     # build --all-targets
    cargo verify-clippy    # clippy --all-targets -- -D warnings
    cargo verify-test      # test --release --bin pgmcp
    cargo smoke            # run --release --example gpu_smoke

## Integration tests: `autotests = false` (`pgmcp-testing/tests/`)

Dropping a new file into `pgmcp-testing/tests/` is **not** enough. The package
sets `autotests = false` and routes all 234 test files through one `[[test]]`
target, so you must add `mod <file>;` to `pgmcp-testing/tests/main.rs` or your
tests are never compiled and never run — silently.

Cargo's default (one `[[test]]` per `tests/*.rs`) meant 234 crates each
statically linking the ~282 MB `libpgmcp` rlib plus candle/cudarc/ort — ≈23.5 GB
of linker output and >1 h of wall time. One target: ~8 min cold, ~12 s to run
1,252 tests. Shared helpers are `crate::common::…` (declared once in `main.rs`).
Don't add process-global mutation (`env::set_var`, `set_current_dir`, global
subscriber init) to a test file — under `cargo test` they all share one process
now. The root `pgmcp/tests/` keeps autodiscovery (Gate 6 names
`--test gpu_fallback_smoke`). Full rationale: `docs/DEVELOPING.md`.

## CUDA is mandatory

pgmcp does not support a CPU-only build. Every build links cudarc, `ort/cuda`,
and the nvcc-generated fused-reduction PTX (`src/fcm/cuda/kernels.cu`, compiled
into `$OUT_DIR/fcm_kernels.ptx` by `build.rs`). The CUDA toolkit (nvcc +
libcudart + libcublas + libcublasLt) must be installed.

At runtime, if CUDA initialization fails (no GPU, driver mismatch,
`CUDA_VISIBLE_DEVICES=""`, etc.), `src/fcm/make_backend()` logs a warning
and returns a `CpuFcmBackend`. The trait `FcmBackend` (in `src/fcm/mod.rs`)
is the seam where a future non-CUDA primary backend (Metal, ROCm, pure-CPU)
could be plugged in without feature gates.

There is no `cuda` cargo feature. `Cargo.toml` has no `[features]` table.

## Session-level mandates (`src/sessions.rs`)

pgmcp observes user prompts via the UserPromptSubmit hook
(`~/.claude/hooks/pgmcp-rag.sh` POSTs `{session_id, cwd, prompt}` to
`POST /api/session/observe`) and extracts imperative directives with a
tiered heuristic regex pipeline calibrated against the user's actual
prompt history. Extracted mandates are persisted by session_id with 12
polarities (always/never/prefer/avoid/remember/from_now_on/correction/
permission/constraint/mandate/process_rule/project_rule) and re-injected
on every subsequent prompt as `additionalContext` to alleviate the LLM's
short-term-memory problem.

The agent can introspect via the `session_mandates` MCP tool and promote
a session mandate to durable scope via `promote_session_mandate`
(inserts into `durable_mandates`; with `write_to_file=true`, appends to
the named target file under a `## Promoted session mandates (pgmcp)`
marker section, idempotent on re-run).

Prompts are persisted locally in `session_prompts` (sha256-deduped,
embedded for cross-session retrieval); same privacy posture as
`file_chunks` — purely local, no remote shipping.

## Work-item / bug tracking (`src/tracker/`)

`src/tracker/` + `src/db/queries/work_items.rs` + `src/mcp/tools/work_items/`
implement a hierarchical work-item tracker (15 kinds) whose trust boundary is
*structural*: an agent can self-report `claimed_done` but **cannot self-verify,
self-defer, or self-confirm** — those transitions have no `Agent` arm in the
`src/tracker/transition.rs` matrix (property-tested). Full design and rationale:
`docs/decisions/004-work-item-tracker.md`.

Bugs are a first-class kind (`kind='bug'`, distinct from the code-marker
`fixme`). A bug is born in `triage` and carries a `severity`
(`critical | high | medium | low`; closed `Severity` enum, `src/tracker/severity.rs`)
plus a 1:1 `work_item_bug_details` sidecar (reproduction / expected-vs-actual /
environment / versions / root cause / resolution). A human confirms a bug with
the user-token `work_item_triage` (`triage → confirmed`, requiring severity +
reproduction); `work_item_resolve` closes one without a fix (`→ cancelled`) with
a categorized `resolution` (closed `BugResolution` enum). `work_item_triage` /
`work_item_resolve` / `work_item_defer` require `[tracker] user_token` — agents
do not have it. Closed vocabularies follow the ADR-003 idiom (TEXT + CHECK built
from the Rust enum's `sql_in_list()` + a golden test pinning the set); the v12
migration is `src/db/migrations/v12_bug_tracker.rs`.

The zazzy-galaxy roadmap (4 phases; addendum in
`docs/decisions/004-work-item-tracker.md`) added trajectories, ergonomics,
close-the-loop, and push:

- **Ergonomics (v16, `src/tracker/views.rs`).** `work_item_view` (smart-views:
  `my-work` / `needs-triage` / `overdue` / `blocked` / `next-actionable`),
  `work_item_next_actionable`, `work_item_assign` (durable `assignee` /
  `assigned_at` / `assigned_by` — owns, vs. the ephemeral `claimed_by` lease;
  never auto-cleared), `work_item_history`, `work_item_bulk` (`BulkOp`:
  `set_status`/`tag`/`untag`/`reprioritize`/`assign`, per-item chokepoint). `SmartView`
  / `BulkOp` are request-shaping enums (no DB CHECK). **Auto-unblock cascade**
  (`src/db/queries/work_items.rs`): verifying an item moves dependents `blocked →
  ready` as `Actor::System` in-tx via `check_transition` — System has no
  judgment-state arm (`system_absent_from_judgment_columns`), so it unblocks but
  cannot complete.
- **Git/PR close-the-loop (v17, `src/tracker/{git_link,commit_ref,auto_transition}.rs`).**
  Commit/PR convention: `#<public_id>` (touch → `in_progress`) or
  `fixes|closes|resolves|implements|refs <public_id>` (close → `claimed_done`).
  The git indexer auto-links + agent-grade auto-transitions (per-project `[git]
  auto_link_items`, default on with `index_history`); `work_item_link_commit` is
  the manual link (`GitLinkType` = `commit`/`pr`/`branch`). **THE TRUST
  BOUNDARY:** a commit/merge runs as `Actor::Agent` and can NEVER reach `verified`
  (no `Agent` arm; `next_auto_status` is exhaustively tested to never return a
  judgment status); it stops at a `verifying` *candidate*. Only CI-posted
  `source='ci'` evidence (`POST /api/tracker/ci_evidence`) flips → `verified` via
  the gatekeeper; `POST /api/tracker/pr_event` stages a merge candidate. The
  `findings-promotion` cron (`src/cron/findings_promotion.rs`,
  `FindingSource`=`bug_prediction`/`documented_tech_debt`) idempotently promotes
  findings → `pending` items (opt-in `[tracker] auto_promote_findings`, default
  OFF; provenance-keyed for idempotency; never pre-`confirmed`).
- **Trajectories (Phase 1).** `quality_trend` / `quality_forecast` +
  `work_item_burndown`'s `slope_per_day` / `regression_eta_days`, over
  `quality_report_history` now filled by the `quality-history` cron
  (`[cron] quality_history_interval_secs`, default 6h). Math:
  `src/quality/forecast.rs` (`ols_slope` / `weeks_to_threshold` / `pct_change`).
- **Proactive digest (v18, `src/digest/`).** Off by default; `[digest] enabled =
  true` surfaces TRACKER+HEALTH+TREND in the SessionStart `pgmcp context` and the
  UserPromptSubmit `additional_context` (channels `session_start`/`prompt`;
  optional `webhook`). **Read-only by construction** — only `SELECT`s plus one
  INSERT into `digest_emissions`; `pgmcp-testing/tests/digest_trust_boundary.rs`
  bans `set_work_item_status`/`Actor::` from `src/digest/`. The
  `pg_notify('pgmcp_digest', …)` seam is wired but off (`[digest] pg_notify`,
  no consumer built).

## Software pattern catalog (`src/patterns/`)

The curated catalog ships ~810 entries across 14 paradigms in 21 per-family
files: `gof`, `solid_grasp`, `principles`, `functional`, `concurrency`,
`architecture`, `declarative`, `anti_patterns`, `code_smells`, `security`,
`testing`, `idioms`, `aop`, `observability`, `deployment`,
`data_engineering`, `api_design`, `ml_ai`, `distributed_data`,
`kubernetes`, and `sources` (registry). `kind` is constrained to
`pattern | anti_pattern | principle | code_smell`. `mod.rs` exposes the
`pat(...)` helper and assembles `pattern_seeds()`. To add a new pattern,
append a `pat(...)` call to the appropriate per-family file; referential
integrity tests in `mod.rs` automatically check slug/paradigm/source/kind
consistency. The current embedding signature is
`pgmcp-pattern-embedding-v3`; bump it whenever seed prose changes so
existing installs re-embed cleanly.

## CUDA host compiler pin (`.cargo/config.toml`)

The CUDA host compiler is force-pinned to `g++-14` via `.cargo/config.toml`
(`NVCC_CCBIN = { value = "g++-14", force = true }`) because GCC 15+ ships
C++23 `<functional>` (explicit object parameters / "deducing this") that
`nvcc` 12.x cannot parse. Without the pin, the `candle-kernels` transitive
build (via `cudaforge` → `nvcc moe_wmma*.cu`) explodes against the system
g++. `force = true` is required because Cargo's `[env]` is non-forcing by
default — a developer-exported `NVCC_CCBIN` would otherwise silently
shadow the project setting and resurrect the build break. Do not remove
the pin or `force = true` without verifying every transitive `.cu` compile
against your system g++. `scripts/verify.sh` preflights for `g++-14` so
misconfigured hosts fail fast with a clear message instead of in Gate 2.

## Architecture: the FCM backend trait

Swappable compute paths live behind `src/fcm::FcmBackend`. Closed
construction-time choices (precision, backend kind) are enums. See
`src/fcm/mod.rs` for the canonical definitions.

- Traits where impls are swappable and may grow (`FcmBackend`).
- Enums where the choice is closed and construction-time
  (`GpuPrecision`, `BackendChoice`, `FcmError`).

## Engineering principles (ADR-022, `src/engprinciples/`)

Four behavioral mandates are enforced uniformly across EVERY MCP client via the
cross-agent seed `src/engprinciples/` (mirrors `src/docguidelines/`): injected
into the MCP `instructions` banner, surfaced in `orient`, the
`engineering_principles` tool, and the `pgmcp://engineering-principles` resource.

1. **Full generality (no overfitting).** Never overfit a solution to one problem
   such that it regresses elsewhere; all solutions must be fully generalized.
2. **Boy Scout rule (fix every bug).** Leave the system better than you found it;
   fix all issues you discover, pre-existing or not; no bug, however rare, is
   acceptable. Mechanically gated by `pgmcp bug-gate` (verify.sh Gate 9).
3. **Capture command output, then clean up.** Pipe validation/compilation/
   evaluation output to files for follow-up analysis, then delete the temp files.
   Mechanically steered by the user-scope `pgmcp-output-capture-enforce.sh`
   PreToolUse hook + the `pgmcp-temp-sweep.sh` Stop hook.
4. **Occam's Razor (simplest, not simpler).** Keep changes as simple as possible
   to accomplish their goal but no simpler; full generality is a requirement, so
   it aligns with Occam — make no extraneous changes.

Two are judgment properties (1, 4) with no mechanical oracle — durable
re-injection is the enforcement; two (2, 3) additionally carry a real gate. See
ADR-022 for the enforcement matrix and the repo-vs-user-scope split.

## Logging level convention (ADR-021)

A caught/swallowed runtime error, or a degraded fallback taken *because* an
operation failed (DB/IO, network/LLM, GPU/model load → CPU, parse/extract
failure, a panicked/aborted task, a cron pass/batch that failed), logs at
`error!`. Only an *expected, benign, by-design* condition logs at `warn!`:
config advisory, "not configured / CLI-mode, skipping" no-op, expected-empty
("produced no topics; preserving prior"), "restart required", a documented
demotion, a **findings/quality report** ("violations detected", "quality below
floor"), a designed budget/latency cap, a graceful documented degradation (LMDB
cold-start, worktree-grouping fallback), or a **trust-boundary "refused"**.

Rationale: at `[logging] level = "error"` (a common production posture) every
`warn!` is silently dropped, so a runtime error mis-logged at `warn!` is
invisible — the failure mode behind the algo-signature-staleness and
index-freshness incidents. Enforced by
`pgmcp-testing/tests/no_swallowed_error_warn.rs` (fails the build on a
swallowed-error `warn!` outside its documented allow-list). If a discovered
swallow is *also* control-flow-wrong, fix the handling and file a `kind='bug'` —
do not just bump the level.

## Why this file exists

On 2026-04-22 an agent added ~1000 lines under `#[cfg(feature = "cuda")]` and
declared the work complete without ever running `cargo build --features cuda`.
30 errors surfaced when the user forced the build. On 2026-04-23 the feature
gate was removed entirely: CUDA became mandatory and the trait-based FCM
backend replaced feature-gated conditional code. `scripts/verify.sh` plus
this file plus the pre-push hook make the old failure mode structurally
impossible — there are no cargo features left to forget.

See
`~/.claude/projects/-home-dylon-Workspace-f1r3fly-io-pgmcp/memory/feedback_feature_gated_build_verification.md`
for the after-action record.
