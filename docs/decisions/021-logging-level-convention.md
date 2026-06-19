# ADR-021: Logging level convention — `error!` for swallowed errors, `warn!` for advisories

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-015 (DB resilience), ADR-018 (cron run history), ADR-019 (index
  freshness). Supersedes nothing.

## Context

pgmcp logs through `tracing`. The effective filter (`src/logging.rs::build_env_filter`)
honors `RUST_LOG` if set, else `[logging] level` (default `info`). A common production
posture is `level = "error"` (or `RUST_LOG=error`) to keep the daemon quiet. Under that
posture **every `warn!` event is silently dropped**, and at `info`/`warn` a mis-leveled
`warn!` is visually indistinguishable from a benign advisory.

Before this ADR, ~82% of `warn!` call sites (≈245 of ≈301) were *caught-and-swallowed
runtime errors* — "X failed", "batch failed", "pass failed", "falling back after a
failure", "task panicked". Two production incidents are directly attributable to this
mis-leveling, both logged as `warn!("… failed")` and therefore invisible at
`level=error`:

1. The topic-clustering **algorithm-signature staleness** bug (ADR-017 addendum): the
   stamping failure was logged at `warn!` and went unnoticed for ~3 weeks.
2. The **index-freshness false-staleness** bug (ADR-019): "Failed to bulk-mark
   `last_verified_at`" was logged at `warn!` and masked the freshness regression.

A runtime error that the code catches and degrades past is exactly the class an operator
must see *even at the quietest level*. A `warn!` cannot deliver that.

## Decision

Adopt a single, enforced convention:

> **`error!`** — a runtime error that was caught and swallowed, or a degraded fallback
> taken *because* an operation failed (DB/IO error, network/LLM failure, GPU/model load
> failure → CPU/disabled, parse/extract failure, a panicked/aborted task, a cron pass or
> batch that failed). The work did not complete; results are missing or degraded.
>
> **`warn!`** — an *expected, benign, by-design* condition: a configuration advisory
> ("`.pgmcp.toml` malformed; ignoring"), a "not configured / CLI-mode, skipping" no-op, an
> expected-empty result ("produced no topics; preserving prior"), a "restart required"
> notice, a deliberate documented demotion, a **findings/quality report** (e.g.
> "invariant violations detected", "quality BELOW floor", "quality regression detected"),
> a designed budget/latency cap, a graceful documented degradation (LMDB centroid cache
> cold-start; worktree grouping fallback), or a **trust-boundary "refused"** (the work-item
> state machine declining an illegal transition — working as designed).

### Discriminator (the rule applied per site)

`KEEP warn!` iff the message describes an *expected/benign/designed* state. `CONVERT to
error!` iff it describes a *caught error the code then swallows or degrades past*. The
tell: words like "failed", "could not", "panicked", "aborting", or "falling back" *after a
real failure* ⇒ `error!`; words like "skipping (CLI mode)", "preserving prior", "not
configured", "restart required", "detected/report", "refused" ⇒ `warn!`.

### Sweep performed (2026-06-19)

≈245 sites converted `warn!`→`error!`; ≈56 retained as `warn!`. The conversion changed
**only** the macro — never the format string, fields, or control flow. Notable retained
`warn!` sites (the discriminator in action):

- `src/fcm/mod.rs` (precision auto-adjust, non-convergence-within-bound),
  `src/cron/gpu_fcm.rs` (fp16→bf16 auto-switch) — algorithmic advisories, results valid.
- `src/embed/pool.rs` (documented extraction-timeout demotion; per-file non-UTF-8 skip;
  transient `lock_timeout`/`PoolTimedOut` retries).
- `src/db/queries/work_items.rs` ("transition refused" — the ADR-004 trust boundary).
- `src/cron/topic_clustering.rs` (degeneracy quality-gate findings; "produced no topics /
  no meta-topics; preserving prior"; LMDB centroid-cache cold-start).
- `src/cron/{memory_eval,retrieval_eval,latent_pipeline_quality}.rs` (findings/quality
  reports: violations detected, quality below floor, regression detected).
- `src/indexer/config_watcher.rs` ("… changed — restart required").
- `src/cli/daemon.rs` (CLI-mode "no PgPool, disabled" no-ops; soft shutdown "did not stop
  within 5s" timeouts; non-loopback bind advisory; "Document extraction tool MISSING").
- `src/mcp/tools/{tool_code_ppr_search,tool_memory_graph_rag}.rs` (designed latency caps),
  `src/mcp/tools/tool_reindex.rs` (expected concurrency/shutdown rejections).
- `src/proc_clients/ebpf.rs` ("bpftrace not on PATH; capture disabled" — environment
  advisory), `src/fuzzy/phonetic.rs` ("no rule pack for language; falling back to
  English" — expected), `src/db/migrations.rs` ("lock contention; retrying after backoff").

## Enforcement (regression guard)

`pgmcp-testing/tests/no_swallowed_error_warn.rs` (modeled on
`no_legacy_chunk_embedding_sql.rs`) recursively scans `src/` and **fails the build** if a
`warn!(` call contains a high-precision swallowed-error phrase (`failed`, `could not`,
`panicked`, `; falling back` / `, falling back`) unless the exact site is on a documented
allow-list (the retained `warn!` sites above whose message legitimately contains such a
word, e.g. "guard failed (non-fatal; continuing)", "no rule pack … falling back to
English"). The allow-list is keyed on a message substring, so a *new* swallowed-error
`warn!` in any file is caught. The test runs inside Gate 5 (`cargo test -p
pgmcp-testing`); no `verify.sh` change is required.

This pairs with the human-facing convention recorded in `CLAUDE.md` ("Logging level
convention"). The test is the teeth; the prose is the explanation.

## Boy-Scout escalation

If a site is *swallowed* **and** *mis-leveled* **and** *control-flow-wrong* (an error
caught, logged, and then execution proceeds as if it succeeded — corrupting state), the
fix is not a level bump: file a `kind='bug'` work-item anchored to the file and repair the
error handling. The two precedents above were exactly this shape — the level was the
symptom, the swallow was the disease.

## Consequences

- **Positive:** runtime failures are visible at the quietest production log level; the
  silent-drop incident class is structurally prevented; `error!` density becomes a
  meaningful health signal.
- **Negative:** `error!` volume rises on a degraded host (by construction — that is the
  point). Operators who previously filtered to `error` to mute the daemon will now see
  the failures they were inadvertently muting.
- **Neutral:** the convention is mechanical and testable; new code is guided by the guard
  test rather than reviewer vigilance.
