# ADR-006: Adjacent tagging for the recursive `AcceptanceCriterion` (serde monomorphization-collector hang)

- **Status:** Accepted
- **Date:** 2026-05-26
- **Related:** ADR-005 (the 1024-only cutover, landed in the same work session)

## Context

The experiment subsystem's `AcceptanceCriterion` (`src/stats/acceptance.rs`) is
a *recursive* enum — composite variants `AllOf(Vec<Self>)`, `AnyOf(Vec<Self>)`,
`Not(Box<Self>)` express criteria like "accept iff ALL of [Welch p<0.05,
Cohen's d ≥ 0.5]". It was derived with serde **internal tagging**
(`#[serde(tag = "type")]`) for a self-describing JSON shape
`{"type":"welch_t","alpha":0.05}` stored as JSONB.

Internal tagging on a recursive enum made `cargo build` of the `pgmcp` crate
take **~2 hours**. (It first errored `E0275` at the default `recursion_limit`;
that was "fixed" by raising the limit to 8192 — which converted a fast error
into an unbounded grind.)

### Diagnosis (empirical, not assumed)

`-Z time-passes` on an isolated repro — the enum alone, nothing else from pgmcp
— pinned the stall to **`monomorphization_collector_root_collections`**, *after*
type-checking (0.05s) and borrow-checking (0.05s) finished instantly. That
explains why `cargo check` passed but `cargo build` hung: `check` stops before
monomorphization; `build` runs it.

**Mechanism:** serde's internally-tagged `Deserialize` buffers the whole object
into `serde::__private::de::Content` to find the tag among the data fields, then
re-deserializes the variant payload out of that buffer via `ContentDeserializer`.
For a recursive variant, each recursion level wraps another `ContentDeserializer`
layer, so the monomorphization collector never finishes enumerating
instantiations. `recursion_limit` only sets how deep it grinds before either
erroring (low) or appearing to hang (high).

**Controlled experiment** — same enum, same `recursion_limit = 8192`, only the
serde attribute changed:

| tagging  | attribute                       | JSON shape                                  | compile     |
|----------|---------------------------------|---------------------------------------------|-------------|
| internal | `tag="type"`                    | `{"type":"welch_t","alpha":0.05}`           | **hung** (killed) |
| adjacent | `tag="type", content="params"`  | `{"type":"welch_t","params":{"alpha":0.05}}`| **0.37s**   |
| external | *(none)*                        | `{"welch_t":{"alpha":0.05}}`                | **0.35s**   |

## Decision

Use **adjacent tagging** — `#[serde(tag = "type", content = "params")]` — on
`AcceptanceCriterion`.

- It keeps a literal `"type"` discriminator key (the property the internal shape
  was chosen for), nesting the variant payload under `"params"`:
  `{"type":"welch_t","params":{…}}`.
- The payload is a *separate* value deserialized through the normal path, so it
  does **not** nest `ContentDeserializer` per recursion level — no
  monomorphization blowup.
- `recursion_limit` is reduced from `8192` back to the long-standing `1024` in
  `src/lib.rs` and `src/main.rs`. The `8192` was masking *this* serde blowup; the
  `1024` stays for an unrelated reason — a large `serde_json::json!` stats-snapshot
  literal in `src/stats/tracker.rs` (~250 fields) exceeds the default-128 macro
  recursion depth.

**In-situ confirmation:** with adjacent tagging the real `pgmcp` crate compiles
in **2m 32s** (was ~2h).

**Rejected alternatives:** internal tagging (the bug); external tagging (works,
but drops the `"type"` key); lowering `recursion_limit` (disproven — internal
tagging is broken at *any* limit: low = `E0275`, high = hang); a hand-written
`Deserialize` (works and keeps the flat shape, but ~80 lines of maintenance for
no benefit over adjacent).

## Consequences

- Criterion JSON is `{"type":"welch_t","params":{…}}` (payload nested under
  `params`) rather than flat. No persisted criteria existed (the subsystem is
  new), so there is no data migration. The only consumers are the experiment MCP
  tools, which (de)serialize via serde and are format-agnostic.
- **Regression guard:** a doc comment on the enum forbids reverting to internal
  tagging and explains why, so the hang cannot be silently reintroduced.
- **General rule for this codebase:** any new *recursive* serde enum must avoid
  internal tagging (`#[serde(tag = "...")]` without `content`). Use adjacent
  tagging, external tagging, or a hand-written impl.
