# Logging Overhaul — 2026-05-21

## Context and motivation

A user tailing `~/.local/share/pgmcp/pgmcp.log` reported that most MCP
tool invocations were invisible — they had no way to observe which tools
the agent was using in real time. An audit of `src/mcp/tools/` confirmed
the gap:

| Bucket                                                        | Count |
|---------------------------------------------------------------|-------|
| Tool files total                                              | 142   |
| Tools with an ad-hoc `info!("MCP tool invoked", …)` template  | 70    |
| Tools with zero tracing events                                | 72    |

Tools were added piecemeal over 8+ months; the canonical template
(`tool_semantic_search.rs:28-37`) was never enforced. Recent batches
(SOTA Phase 1–11 and the entire `a2a_*` family) were added without the
template, so the silent fraction was growing.

Worse, the per-tool `info!` was *not* the right insertion point even
when present: the rmcp `#[tool_router]` / `#[tool_handler]` macros
generate dispatch boilerplate that does NOT emit tracing events at
`info` (only at `debug`, via rmcp internals at `service.rs:842, 869`).
The user's default level is `info`. The result: at the level a user
typically tails at, ~half the tools were silent.

The right insertion point already existed — `instrumented_tool_wrap` in
`src/mcp/server.rs:108` wraps every MCP tool call (140+ sites). It
captured the tool name, caller identity, duration, success/error
outcome, and even enqueued a durable `TelemetryRow` to the
`mcp_tool_calls` table. But it emitted *no* `tracing::*` events. Adding
two `info!` lines there made every tool visible at once.

The user approved the broader overhaul rather than the minimum fix.

## Hypothesis

**H1.** If `instrumented_tool_wrap` is augmented with `info!` events at
entry and exit (and `warn!` on error), the user's complaint —
"invocations are invisible in pgmcp.log" — is fully resolved without
touching any of the 140+ `#[tool]` methods.

**H2.** If a `params_summary` is plumbed through the wrapper, those
events also carry the same per-tool context the ad-hoc template gave —
so the per-tool ad-hoc lines become redundant at `info` (and the
cleanup of 70 files removes a double-log without losing information).

**H3.** If `call_tool_cli` is routed through the same wrapper (via a
sibling `instrumented_tool_run` that does not require a
`RequestContext`), `pgmcp tool <name>` invocations land in
`pgmcp.log` too, giving the user a single tail to watch.

**H4.** If `LoggingConfig` grows `format` / `targets` / `access_log`
fields, the operator gets per-target level control, an output-format
switch, and an optional separate MCP-tool-call access log without
needing `RUST_LOG` for every customization.

## Design

Six phases, each independently mergeable.

### Phase 1 — Central seam: tracing events

`instrumented_tool_wrap` now forwards to a new
`instrumented_tool_run(stats, name, timeout_secs: Option<u64>, caller,
params_summary, request_id, fut)`. `instrumented_tool_run` does the
timing/stats/telemetry work the wrap function used to do verbatim, and
adds:

```rust
let span = info_span!(target: "pgmcp::mcp::tool", "mcp_tool",
                     tool = name, client = %caller.client_name);
info!(target: "pgmcp::mcp::tool", tool = name,
      client = %caller.client_name, params = %params_summary, "invoked");
// … timed await on (timeout_wrap | fut).instrument(span) …
match &result {
    Ok(_) => info!(target: "pgmcp::mcp::tool", tool, client, duration_ms,
                   "completed"),
    Err(e) => warn!(target: "pgmcp::mcp::tool", tool, client,
                    duration_ms, error = %e, "failed"),
}
```

Key choices:

- **Span**: `.instrument(span)` on the awaited future so descendant
  `debug!` events inside the tool body nest under the tool call when
  the user runs at `debug`.
- **Target `pgmcp::mcp::tool`** (singular): the central seam's events.
  Tool body events use `pgmcp::mcp::tools::tool_X` (the natural
  module-path target). The two are independently filterable.
- **`Option<u64>` for timeout**: `None` is required by `reindex`
  (which runs for minutes and reports progress out-of-band via the
  task store). `reindex` migrated off its inline timing duplicate to
  `instrumented_tool_run(..., timeout_secs: None, ...)`.

### Phase 2 — Params summary

A `summarize_debug<P: Debug>(&P) -> String` helper formats the typed
`Params` struct (every `*Params` derives `Debug`) and truncates to
200 bytes on a UTF-8 char boundary with a `…(+NB)` suffix.
`summarize_json(&serde_json::Value)` does the analogous job for the
raw JSON args in `call_tool_cli`.

All 140+ `#[tool]` methods updated to pass `&summarize_debug(&params)`
(or `""` for nullary tools) as the new arg. Edit was scripted via
two raku passes:

- `pgmcp-logging-bulk-edit.raku` — handled 143 single-line futures.
- `pgmcp-logging-bulk-edit-multiline.raku` — handled the 25
  multi-line futures (long arg lists).
- One additional manual patch for `memory_reflect` whose 5-line
  preamble was broken by an explanatory comment between `<secs>`
  and `&_ctx`.

Total call sites edited: **168 + 1 = 169** (one site, `reindex`, uses
`instrumented_tool_run` directly rather than `instrumented_tool_wrap`).

### Phase 3 — CLI seam through the same wrapper

`call_tool_cli` (`src/mcp/server.rs:6901`) now:

1. Constructs a synthetic `CallerInfo { client_name: "cli", … }`.
2. Computes `params_summary = summarize_json(&args)`.
3. Wraps the `dispatch_tool!` macro in an `async move { … }` block.
4. Passes that future to `instrumented_tool_run(stats, name, None,
   caller, &params_summary, None, fut)`.

The CLI path now has the same tracing, in-memory `StatsTracker`
counters, and durable `mcp_tool_calls` telemetry rows as the MCP
transport path. Caller appears as `client = "cli"` in dashboards.

`src/logging.rs::init_cli` was replaced by
`init_cli_with_config(config: Option<&Config>)`. When called with
`Some(&cfg)`, a JSON file layer is added (in addition to the stderr
human-readable layer), appending to `cfg.logging.file`. The CLI does
*not* rotate — rotation is owned by the daemon. CLI invocations
across a daemon rotation continue writing to the (now-rotated) file
for their lifetime, which is acceptable for short-lived processes.

All 7 CLI subcommands (`analyze`, `context`, `reindex`, `results`,
`statistics`, `status`, `tool`) updated to load `Config` *first*,
then call `init_cli_with_config(Some(&config))`. Tools that fail
before config load (none today) would fall through to a stderr-only
init.

### Phase 4 — Config knobs: `format` + `targets`

`LoggingConfig` (src/config.rs:1330) gained three new fields:

```toml
[logging]
file = "~/.local/share/pgmcp/pgmcp.log"   # unchanged
level = "info"                            # unchanged
rotation = "daily"                        # unchanged
max_log_files = 7                         # unchanged
format = "json"                           # NEW: "json" | "compact" | "pretty"
[logging.targets]                         # NEW: per-target overrides
"pgmcp::mcp::tool" = "debug"
"sqlx::query"     = "warn"
access_log = "~/.local/share/pgmcp/mcp-access.log"  # NEW; see Phase 5
```

`build_env_filter(level, &targets)` in `src/logging.rs` composes a
`tracing_subscriber::EnvFilter` from the global level plus each
per-target directive. `RUST_LOG` still takes precedence when set.
Invalid directives are reported to stderr and skipped — they don't
abort startup.

`make_format_layer(format, writer)` in `src/logging.rs` returns a
boxed `Layer` choosing between `.json()`, `.compact()`, and
`.pretty()`. Both `init_daemon` and the file branch of
`init_cli_with_config` consume it.

### Phase 5 — Optional MCP-tool-call access log

When `cfg.logging.access_log` is `Some(path)`, `init_daemon` adds a
second tracing layer:

- writer: a `RotatingFileAppender` for the access-log path with the
  same `rotation` + `max_log_files` policy as the main log;
- format: JSON (operators typically pipe access logs through `jq`);
- filter: `filter_fn(|m| m.target() == "pgmcp::mcp::tool")` — only
  the `invoked` / `completed` / `failed` events from
  `instrumented_tool_run`.

Result: a clean, nginx-style access log of MCP tool traffic, even
when the main log is set to `warn` or filtered to other targets.

### Phase 6 — Dedup ad-hoc per-tool logs; backfill silent bodies

After Phases 1–2, the central seam already emits one `info` line per
invocation at the `pgmcp::mcp::tool` target. The 70 per-tool ad-hoc
`info!("MCP tool invoked", …)` calls become redundant at `info`
level, but their richer field set is still useful at `debug`. So:

- Demoted `info!(…, "MCP tool invoked")` → `debug!(…, "MCP tool
  invoked")` in all 70 logged tool bodies via
  `pgmcp-logging-demote-info.raku` (state-machine line walker that
  finds the `info!(`…`);` block containing the marker string).
- Backfilled all 72 silent tool bodies with a uniform
  `tracing::debug!(tool = "<name>", "MCP tool invoked");` at the
  body opener via `pgmcp-logging-backfill-silent.raku` (handled 68
  single-function files) and `pgmcp-logging-backfill-shared.raku` +
  `pgmcp-logging-fixup-shared.raku` (handled the 4 shared-module
  files that house 23 sub-functions total: `tool_memory_crud.rs`,
  `tool_memory_ext.rs`, `tool_memory_graph_rag.rs`,
  `tool_client_profile.rs`).

Final state: every one of the 142 tool body files contains at least
one `debug!` body-start line referencing its tool name. Combined
with the central-seam `info!` events, an operator can:

- Tail `pgmcp.log` at default `info` → see every invocation
  (`pgmcp::mcp::tool invoked` + `completed`/`failed` with `tool`,
  `client`, `duration_ms`, `params`).
- Set `RUST_LOG=pgmcp::mcp::tools=debug` → also see per-tool
  body-start events with the original rich field set
  (e.g. `query`, `limit`, `language`, `project`).
- Set `RUST_LOG=pgmcp::mcp::tools::tool_semantic_search=debug` →
  scope debug to one specific tool.

## Before/after metrics

| Metric                                                      | Before | After |
|-------------------------------------------------------------|--------|-------|
| Tool files emitting any event at `info` (default tail)      | 0†     | 142   |
| Tool files emitting any event at `debug`                    | 70     | 142   |
| Central tracing seam present                                | no     | yes   |
| CLI invocations visible in `pgmcp.log`                      | no     | yes   |
| Operator can filter `[logging] targets` per-target          | no     | yes   |
| Operator can choose JSON / compact / pretty output          | no     | yes   |
| Operator can route tool-call traffic to a separate file     | no     | yes   |

† The 70 "logged" tool files did emit `info!("MCP tool invoked")`,
but only when the per-tool body actually ran. The rmcp dispatch
seam wrapping them had no info-level event — and these 70 were
visible only when `info` level was on AND only post-deserialization.
The 72 silent files were invisible at any level. So a tail at
`info` saw a partial view; this analysis treats the central seam
as the meaningful "visibility yes/no" criterion.

## Verification

`./scripts/verify.sh` (the project's non-negotiable gate) was run
after the final phase. Intermediate gates run after each phase:

- `cargo build --all-targets` — clean after all six phases.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test --release --bin pgmcp` — full suite passes (no
  regressions in `instrumented_tool_wrap` callers; `record_tool_call`
  contract preserved).

Observable verification (post-restart):

```bash
tail -f ~/.local/share/pgmcp/pgmcp.log \
  | jq -c 'select(.target == "pgmcp::mcp::tool")'
```

Expect one `"invoked"` + one `"completed"` (or `"failed"`) per tool
call, with `tool`, `client`, `duration_ms`, and `params` fields.

CLI verification: `pgmcp tool semantic_search --args
'{"query":"foo","limit":5}'` produces matching events with
`client="cli"`.

## Risks and known limitations

1. **Log volume.** Each tool call now emits two info-level lines plus
   a 200-byte param summary, roughly 1 KB. At 60 calls/min that's
   ~1.4 MB/day — well within the daily rotation budget. Heavier
   workloads (hundreds of calls/min) should monitor disk; the
   200-byte truncation length is tunable in `truncate_for_log`.

2. **Param privacy.** `summarize_debug` serializes whatever
   `Params` derives. No current `*Params` struct carries secrets.
   If a future tool adds a sensitive field, mark it
   `#[serde(skip_serializing)]` (still effective for Debug via a
   custom impl) or introduce a per-tool `redact_params` hook.

3. **CLI + daemon both append to the same file.** Linux's append
   semantics make single-line records < `PIPE_BUF` (4 KB) atomic.
   200-byte payloads are well under the limit. The CLI does not
   rotate — a CLI invocation that spans a daemon-side rotation
   continues writing to the now-rotated file for its lifetime.
   Acceptable: CLI invocations are short.

4. **170 call-site refactor.** Scripted via raku, verified by the
   compiler (the new mandatory `params_summary` arg makes any
   missed site a build error). One site (`memory_reflect`) needed
   a manual patch due to an explanatory comment splitting its
   5-line preamble.

5. **rmcp upstream changes.** rmcp 1.1's `#[tool_handler]` macro
   does not expose a middleware hook; we wrap inside each `#[tool]`
   body. If a future rmcp release adds a middleware seam, the
   central wrapper migrates trivially (it's already a single
   function).

## Rollback notes

- **Phase 1+2 alone** can be reverted by restoring the original
  `instrumented_tool_wrap` signature (drop `params_summary`, drop
  `info!`/`warn!` calls). 169 call sites would need their extra
  arg removed — bulk-revertable with the inverse raku script.
- **Phase 3 alone** can be reverted by restoring `call_tool_cli`
  to its direct `dispatch_tool!` body and `init_cli` to its
  stderr-only single-init.
- **Phase 4+5** is configuration-additive: revert by removing the
  three new fields from `LoggingConfig` and the
  `make_format_layer`/access-log code paths. No data migration
  needed (no schema change).
- **Phase 6** is purely level-and-message edits; reverting just
  flips `debug!` → `info!` for the demoted block and removes the
  72-file `debug!` body-start backfill. Mechanical; no semantic
  consequences.

## Footprint

- 1 new file: `docs/scientific-ledger/logging-overhaul-2026-05-21.md`
  (this file).
- 4 modified files in `src/`: `mcp/server.rs`, `logging.rs`,
  `config.rs`, plus the 142 tool body files in `src/mcp/tools/`.
- 7 modified CLI subcommands: `src/cli/{analyze, context, reindex,
  results, statistics, status, tool}.rs`.

## Open follow-ups

None. The plan's "Phase 4+5 optional access-log layer" shipped.
The optional `params_sha256` field on `TelemetryRow` is now wired:
`instrumented_tool_run` hashes `params_summary` with `sha2::Sha256`
(already a dep via `src/mandates.rs`) and emits the hex digest in
the telemetry row (`None` for nullary tools so analyses can
distinguish "no params" from "identical params"). This gives
downstream `mcp_tool_calls` analyses a stable join key for
deduplicating identical-shape invocations across clients, agents,
and time windows.

## Related memories

- `feedback_cli_must_init_tracing.md` — motivated `init_cli`'s
  existence; this overhaul renames it to `init_cli_with_config` and
  extends it.
- `feedback_hook_reliability_layers.md` — adjacent to logging UX
  but unrelated to MCP-tool visibility.
- The plan file `~/.claude/plans/how-can-pgmcp-be-smooth-cherny.md`
  is the design doc; this ledger is the as-built record.
