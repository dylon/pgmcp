# ADR-016 — Token economy: adaptive per-client tool surface + result-payload overhaul

- **Status:** Accepted, implemented 2026-06-13
- **Scope:** `src/mcp/{client_profile,tool_policy,tool_domains}.rs`,
  `src/mcp/server.rs`, `src/mcp/server/handlers/meta.rs`,
  `src/mcp/tools/{tool_meta,result_shaping}.rs`, `src/db/mcp_tool_catalog.rs`,
  `src/db/migrations/v37–v39`, `src/cron/tool_policy_refresh.rs`,
  `src/cron/embedding_migration.rs`, `src/stats/telemetry_writer.rs`,
  `src/db/queries/search.rs`, `src/mcp/params/{search,meta}.rs`,
  `src/mcp/tools/sema_helpers/effects.rs`,
  `src/mcp/tools/tool_mcp_tool_telemetry.rs`, `src/context.rs`, `src/config.rs`,
  `src/cli/daemon.rs`
- **Relates to:** ADR-003 (closed-vocabulary enum idiom), the per-client
  protocol customization (`client_profile.rs`, "Memory-server Phase 10").

## Context

pgmcp exposes ~330 MCP tools. Every `tools/list` response ships the full
catalog — name + description + JSON input-schema for all of them — at ~480
tokens each, i.e. **~150 K tokens loaded into every client conversation up
front, before any work happens.** That fixed cost dwarfs everything else; the
per-call result path was already partly optimized (per-client `CompactJson`
exists for codex, the UserPromptSubmit additional-context is hard-capped at
2 KB), but the result payloads themselves were still pretty-printed and
unbounded.

Two levers, both pursued here:

1. **Tool definitions (dominant).** Shrink what `tools/list` returns per client.
2. **Results (secondary).** Make every tool result honor the caller's compact
   format and slim the heavy search payloads.

## Decision — Part 1: adaptive per-client tool surface

A per-session, per-client tool set selected at the single `list_tools`
chokepoint (`server.rs::list_tools`):

```
exposed(session) = mandatory_core ∪ learned_defaults(client) ∪ session_enabled(session)
                   (ToolSurface::All short-circuits to the full catalog)
```

- **`ToolSurface`** (`client_profile.rs`, closed enum per ADR-003): `All`
  (claude-code's default — the filter is a no-op, so `tools/list` is
  byte-identical to the unfiltered router), `Learned` (the token-sensitive
  default for codex / generic / unknown clients), `Fixed(Vec<domain>)` (manual
  override). `mandatory_core` defaults to `DEFAULT_MANDATORY_CORE` (~16 tools:
  the discovery/expansion meta-tools + the first-reach search/read/memory core).
- **Usage-adaptive defaults** (`tool_policy.rs`). The `tool-policy-refresh`
  cron scores every `(client, tool)` by an exponentially **recency-decayed usage
  frequency** — `w = Σ exp(-age_days / τ)`, τ=14d, included when `w ≥ θ=0.5` —
  over the durable `mcp_tool_calls` telemetry, materializes the scores into
  `client_tool_policy` (v37), and hot-swaps an in-memory `ToolPolicySnapshot` (an
  `ArcSwap` on `SystemContext`). Cold start (no client history) falls back to a
  global most-used prior. This is a **deterministic recency-weighted frequency
  estimator (an EWMA of the usage impulse train), not a trained/parametric ML
  model** — τ and θ are fixed constants, nothing is fit by minimizing a loss;
  "learned" denotes online adaptation (as in an LFU-with-aging cache). The full
  derivation — half-life, the steady-state identity `E[w]=λτ`, the
  threshold-as-rate-gate `λ ≥ θ/τ`, truncation error, and the relationship to
  EWMA / time-decayed stream aggregates / Hawkes background intensity — is in
  **`docs/design/tool-policy-recency-decay.md`**. **The feedback loop:** every
  `enable_tools` call and every native call is itself telemetry, so
  frequently-used tools are promoted into the default set on the next pass and
  unused ones decay out — the surface converges to each client's real working set
  with zero manual curation.
- **Dynamic expansion** (`server/handlers/meta.rs`). Four meta-tools, always in
  the mandatory core:
  - `tool_catalog(query?, domain?)` — semantic/keyword browse over the server's
    own tools (`mcp_tool_catalog`, v38, embedded by the embedding-migration cron
    like `tool_cards`; keyword `ILIKE` fallback until embeddings backfill).
    Returns names + one-line descriptions, NOT full schemas.
  - `enable_tools(names?, domain?, query?)` — unions the resolved tools into the
    caller's per-session overlay (`SystemContext.tool_sessions`, keyed by the
    `mcp-session-id` header, TTL/LRU-bounded) and emits `tools/list_changed`
    (capability now enabled) so the client re-fetches `tools/list` and the tools
    appear **natively**.
  - `disable_tools(names?, all?)` — symmetric.
  - `call_tool(name, args)` — generic dispatch fallback reusing the existing
    `dispatch_tool!` table (factored into `dispatch_named` /
    `dispatch_for_call_tool`), attributed to the *inner* tool so it also teaches
    the learner. The robustness net for clients that ignore `list_changed`.

**The trust/scope boundary:** gating only hides tools from `tools/list`; it never
makes a tool unreachable (`call_tool` always works), so a non-cooperating client
degrades gracefully rather than losing capability.

**rmcp feasibility:** the keystone is `extract_mcp_session_id` (already present —
the `mcp-session-id` header off the streamable-HTTP transport) for the
per-connection key, plus rmcp 1.1.0's `Peer::notify_tool_list_changed` +
`ServerCapabilities::enable_tool_list_changed`. No fork required.

**Measured result** (unit test `learned_surface_shrinks_real_tools_list_…`):
a `Learned` client with no history sees only its ~16-tool core — a **>80%
`tools/list` byte reduction** — while `All` (claude-code) is byte-for-byte
unchanged.

## Decision — Part 2: result-payload overhaul

- **Central re-encoding** (`server.rs::reencode_result_for_format`, applied at the
  `call_tool` dispatch boundary). For `CompactJson` callers, each pretty-printed
  JSON text block is parsed and re-serialized compact — trimming ~30-40%
  whitespace across EVERY tool (the ~88 using `sota_helpers::json_result`, the
  handful with their own `json_result`, and the ~87 inlining `to_string_pretty`)
  without editing 300+ bodies. Non-JSON text and the `Markdown` posture are
  no-ops, so claude-code output is unchanged. A request-scoped `RenderCtx`
  task-local (installed once in `call_tool`) lets `sota_helpers::json_result`
  additionally emit compact at the source for its ~88 tools.
- **Search-result slimming** (`result_shaping.rs`, applied in
  semantic/text/grep): `snippet_length` truncates the unbounded content field
  (default ~500-char preview for `default_brief` clients), `fields` projects to a
  subset, and `default_brief` clients drop the redundant `relative_path` /
  `project_name`. `SearchResult.score` is serialized rounded to 4 decimals.
- **`effect_breakdown`** changed from an array-of-`{effect,count}` to a compact
  `{effect: count}` object map (`effects.rs`), dropping the repeated keys across
  the ~37 sema-enriched tools.
- **Result-size telemetry** (v39: `mcp_tool_calls.result_bytes` /
  `result_tokens_est`, recorded in `instrumented_tool_run`). The
  `mcp_tool_telemetry` tool's new `output_bytes` aggregation surfaces the top
  tools by serialized size per client — the data-driven targeting view for
  further slimming.

## Consequences

- **claude-code (the primary interactive client) is unchanged by default** — it
  receives the full catalog (`All`) and rich Markdown output. The token savings
  accrue to codex / programmatic / unknown clients, which is the explicit intent.
  `claude-code`'s policy is a one-line config flip (`tool_surface = "learned"`)
  if lean-by-default is ever wanted.
- The adaptive surface is **self-tuning**: no hand-curated allowlists; the cron
  learns and the dynamic meta-tools let any client reach anything on demand.
- New per-boot activation: the v37–v39 migrations apply, the `tool-policy-refresh`
  cron registers, `mcp_tool_catalog` warms + embeds, and the policy snapshot
  loads from `client_tool_policy` — all on the next daemon restart.
- **Risk — `list_changed` client support:** a client that ignores the
  notification keeps its lean default set, but `call_tool` (and the next
  `tools/list`) still reach every tool, so capability is never lost.
- **Risk — per-session state lifecycle:** the `tool_sessions` overlay is bounded
  (`MAX_TOOL_SESSIONS`) and TTL-pruned in `enable_tools`.

## Verification

`./scripts/verify.sh` (all gates) plus targeted tests: the headline
token-regression unit test (`tool_policy.rs`), the domain-map consistency test
(`tool_domains.rs` — guards that every assembled tool has exactly one domain),
`result_shaping` slimming tests, the `client_profile` `ToolSurface` / `RenderCtx`
tests, and the real-DB `adaptive_tool_surface.rs` integration tests
(`tool_catalog` dispatch + the learner promote/decay end-to-end). The
`effect_breakdown` object-map shape change updated four golden/oracle tests.
