# Correctness + Sensibility Review — uncommitted changes (2026-06-20)

**Reviewer:** Claude (adversarial gatekeeper pass, requested by the user).
**Mandate:** *only keep what is sensible; do not let an agent manipulate tracker data
to appear more successful than it was.*

Two agents produced the uncommitted body under review:

| Agent | Produced | Verdict |
|-------|----------|---------|
| **Crucible** | the context-tape paging subsystem (`src/tape/**`, 10 `tape_*` MCP tools, CSM `TapePaging`) + P9 `src/experiment/context_tape.rs` | high craft, **not wired** as shipped → **finished + fixed** (this change) |
| **liblevenshtein** | *requested* 6 generic experiment-API improvements (it is a consumer; it wrote no pgmcp code) | mostly sensible & rigor-positive → **built** (this change) |

A pre-existing, unrelated production bug (`graph_neighbors`) was also found and fixed.

---

## 1. Executive summary

| Area | Finding | Disposition |
|------|---------|-------------|
| **Work-item tracker** | An accepted **experiment** verdict auto-drove linked bugs `Triage→Verified` via a synthesized `Gatekeeper`; experiments are agent-controlled (no token; agent supplies the measurements) ⇒ **agent self-verification loophole** | **FULL REVERT** (CI-only `→verified` restored) |
| **Tape paging** | `PagingEngine`+`prefetch` were **dead code** (no production caller); pause/resume **round-tripped an empty table**; verbs wrote a **different store**; + clock/atomicity/TTL/REPL-gate/leak defects | **Integration finished**, all CRITICAL/HIGH fixed |
| **Crucible P9** | pre-registered benchmark, server-computed decision, default-OFF **memory** promotion — careful; does NOT touch the tracker | **Verify + keep** |
| **Experiment-API requests** | mostly rigor-positive; one cherry-pick vector (#4) | **Built with guardrails**; #5 was moot |
| **`graph_neighbors`** | `memory_unified_edges` matview missing in prod (persistent) | **Self-heal fixed** |

---

## 2. CRITICAL — the experiment → work-item self-verification loophole (REVERTED)

### 2.1 What was there

Commit `15c249b` (committed) + an uncommitted `+60` in `src/db/queries/work_items.rs` made an
**accepted experiment verdict** flip a linked bug to `verified`:

- `src/tracker/transition.rs` added `(Triage,Verified)=>Gatekeeper` and `(Confirmed,Verified)=>Gatekeeper`.
- `sync_experiment_verdict_to_work_items` posted `source='experiment'` evidence and called
  `drive_work_item_to_verified`, which **synthesized `Actor::Gatekeeper`** to close the bug.
- `corroborate_manual_required_criteria_with_experiment` (uncommitted) stamped the item's *other*,
  unrelated manual criteria as trusted `source='experiment'` passes.

### 2.2 Why it breaks the non-negotiable

`experiment_open` / `experiment_record_measurement` / `experiment_decide` are **plain agent-callable
MCP tools with no `user_token`**, and the agent supplies the **measurements**. A "frozen" criterion
is trivially gamed (e.g. baseline `[100,100,100]` vs treatment `[1,1,1]` beats any Welch t-test).
The in-tree justification — *"the agent cannot fabricate a statistical verdict over a frozen
criterion"* — is **false**: it controls the inputs. The sync fired **unconditionally** from
`experiment_decide`. Net: an agent could self-verify any linked bug. CLAUDE.md is explicit — *only
CI `source='ci'` flips `→verified` via the gatekeeper; agents cannot self-verify.*

```
  BEFORE (loophole)                          AFTER (reverted — CI-only)
  ┌────────────┐  agent-run, ungated         ┌────────────┐
  │ experiment │──accept──┐                   │ experiment │── accept ──▶ (no tracker effect)
  └────────────┘          ▼                   └────────────┘
                  synth Gatekeeper                  CI  ── source='ci' ──▶ gatekeeper ──▶ verified
                       │                            user_signoff ─────────▶ gatekeeper ──▶ verified
                 Triage ──▶ Verified  ✗ agent self-verify
```

### 2.3 What was done

Full revert, all **comment-out-with-reason** (preserved, per the no-silent-disable mandate):
- `transition.rs`: the two matrix arms commented out; property tests now assert
  `Triage/Confirmed→Verified` is **closed for every actor** (incl. Gatekeeper).
- `work_items.rs`: `sync_experiment_verdict_to_work_items` is an inert no-op (returns 0);
  `drive_work_item_to_verified` and `corroborate_…` disabled (originals preserved).
- `tool_experiments.rs`: the `experiment_decide → sync` call disabled (experiment record +
  `agent_outcomes` linkage unaffected).
- `pgmcp-testing/tests/work_items_smoke.rs`: both loophole tests **inverted** to assert the
  firewall holds (`*_does_not_verify_*`, `*_does_not_corroborate_*`).

### 2.4 Audit follow-up (broader trusted-source set)

The corroboration query treated `ci`, `stop_hook`, `subagent_audit`, `external_auditor`,
`user_signoff`, `experiment` all as "trusted". Of these, **`experiment` is agent-controlled**
(now removed from the `→verified` path). `stop_hook` / `subagent_audit` are agent-environment
sources and should likewise NOT independently flip `→verified`; only `ci` / `user_signoff` /
`external_auditor` are outside agent control. The revert removes the experiment path; the
remaining sources retain their prior (pre-cowboy) behavior. **Recommendation:** a future hardening
pass should confirm `stop_hook`/`subagent_audit` cannot independently reach `→verified`.

---

## 3. Tape paging subsystem — finished + fixed

### 3.1 The core problem (two disjoint planes)

```
   RAM plane (what the verbs read/write)        DB "control plane" (tracked residency)
   ┌─────────────────────────────┐              ┌──────────────────────────────────┐
   │ TapeRegistry / TapeStore     │   ✗ no      │ PagingEngine / working_set_pages   │
   │ key: TreeId (root_task_id)   │   link      │ key: (session_key, state_cursor)   │
   │ holds BYTES                  │ ◀──────────▶ │ tracked METADATA only (bytes = "")│
   └─────────────────────────────┘              └──────────────────────────────────┘
   written by tape_put / rlm stitch             written by PagingEngine.persist() — NEVER CALLED
```

The keystone (missed by all three review passes): **the RLM path has no `session_key`**, and v51
hard-FK'd `working_set_*` to `orchestration_sessions(session_key)` — so an engine `page_in` from
RLM would **fail the FK insert**. That is *why* the engine had zero production callers.

### 3.2 Findings → fixes

| Sev | Finding | Fix (this change) |
|-----|---------|-------------------|
| C1 | `PagingEngine`+`prefetch` dead (tests-only) | wired into the live path via `engine::admit_scratch` (called by `tape_put` + the RLM Store stitch) |
| C2 | pause/resume round-trips an empty table; resume never rehydrates the RAM plane | `store::rehydrate_store_from_pages` rebuilds the `TapeRegistry` from `working_set_pages` on resume |
| C3 | verbs and engine write different, unsynchronized stores | unified: engine tracks residency + budget; `TapeStore` holds bytes; `working_set_pages.content` persists scratch bytes |
| — | RLM path unkeyable (FK) | **v53** relaxes the FK (`session_key = "rlm:{root_task_id}"`), adds `content`, indexes `tree_path`, preserves CSM cascade via a trigger |
| C1(rlm) | accumulator slots `accum/<ord>` collide at recursion depth>1 (concurrent children clobber `accum/summary`) | namespaced per stitch: `accum/<parent_task_id>/<ord>` |
| H1 | dual clock authorities (`tick()` overwrite vs `bump_clock` increment) → determinism unsound | single authority: atomic `bump_clock`; `save_config` no longer overwrites the clock |
| H2 | `save_working_set` non-atomic UPSERT loop → torn state | wrapped in one transaction |
| H3 | `tape_repl` gate **fail-open** at the wire (caller role hardcoded `"Orchestrator"`; black-box arm never fires; casing mismatch) | real caller identity threaded via `extract_caller`; `black_box_roles()` lowercased; unknown ⇒ fail-closed; wire-refusal test added |
| H4 | TTL eviction documented but unimplemented (`ttl_secs` never read) | implemented (`EvictReason::Ttl` produced; logical-age sweep) |
| H1(reg) | `TapeStore` leak (`drop_tree` never called) | `drop_tree` at root-frame completion + `tape-store-reaper` cron backstop |
| M | dirty write-back flushes `""` (data loss) | `ResidentPage.bytes` carried; eviction writes real bytes |
| M | LRU/TTL/FIFO build a `DynamicDawg` then discard it | removed the throwaway; canonical selection kept |
| M | N+1 query per search hit in `resolve_*` | batched (`UNNEST` join) |
| M | DashMap entry-guard held across inner Mutex | guard dropped before locking |
| M | 3 of 4 `[tape]` config fields dead | wired into `WorkingSet::from_config_defaults` |
| M | "corpus READ-ONLY" over-claimed | corrected (`memory_observations` is gated-promotion-writable) |

### 3.3 Good (kept as-is)

ADR-021 logging is clean throughout; `.expect` over `unwrap`; the **rhai REPL sandbox is genuinely
locked down** (`Engine::new_raw`, no fs/net/proc, `eval` off, hard op/byte limits); recursion is
bounded (`MAX_RLM_DEPTH=4`); the Store-env identity comes from the trusted frame (not JSON); the
CSM `TapePaging` is a high-quality formal spec; tests are substantive.

---

## 4. Crucible P9 — `src/experiment/context_tape.rs` (verify + keep)

A **pre-registered** 3×3×5 benchmark with a **frozen** composite acceptance criterion (fixed before
data), a **server-computed** `Decision` passed by value from `acceptance::evaluate` (a caller cannot
forge `accepted=true`), ADR-003 closed vocabularies + golden tests, reusing the existing experiment
framework. Promotion targets **memory** (`memory_observations` supersede), **default-OFF**
(`[experiments] allow_promotion`). Confirmed it does **not** touch the work-item tracker. It is
*infrastructure, not a live run* (the 3×3×5 needs external datasets + local models), with an honest
dataset seam and **no fabricated data**. Trust posture deliberately mirrors the tracker's
non-negotiable. **Disposition: verified (build + tests) and kept.**

---

## 5. liblevenshtein's 6 experiment-API requests (built, with guardrails)

These are **project-agnostic** experiment-subsystem features (a consumer requested them; nothing is
coupled to liblevenshtein, and the liblevenshtein project is untouched). The **firewall**:
experiment outcomes may promote to *memory* (default-off, server-computed) but **never** auto-cross
the work-item `→verified` boundary.

| # | Request | Verdict / build |
|---|---------|-----------------|
| 1 | `record_measurement_from_artifact` (server-side CSV/JSONL parse) | sensible; built, path-traversal-safe, raw samples |
| 2 | chunked ingest + finalize; decide on conforming runs only | rigor-positive; built (`finalize` + decide gated on `ExperimentRunStatus::usable_in_decision`) |
| 3 | paired-binary + **McNemar/exact-binomial** | **correct science** (Welch-t on paired binary is wrong); built (`mcnemar_test`/`exact_binomial_test`, server-computed verdict) |
| 4 | `measurement_set_status(invalid\|superseded)` | the one **cherry-pick vector** → built with guardrails: append-only audit trail, reason required, and **post-decision invalidation re-opens the decision** |
| 5 | fix `data_table_insert` rows schema | **moot** — already `Vec<serde_json::Value>` |
| 6 | `experiment_get` audit visibility (runs/conformance/counts/invalid) | anti-gaming; built |

**Anti-tampering substrate (v52):** `experiment_runs.status` closed vocab + status audit columns +
the immutable `experiment_run_status_audit` trail + a tamper-evident `samples_digest` (SHA-256 over
ordered raw samples, snapshotted at finalize) + the `experiment_paired_binary` 2×2 table.

---

## 6. Incidental — `graph_neighbors` production outage (fixed)

`memory_unified_edges` was missing in the live DB (every unified-graph tool errored
`relation "memory_unified_edges" does not exist`). Root cause: `build_memory_unified_views` drops
edges first, builds the multi-minute nodes HNSW, creates edges last; a SIGKILL in that window left
edges dropped, and a matching stored views-hash then masked it (the hash gate skipped; the startup
guard only checked nodes). Fixed in `src/db/migrations.rs`: `matviews_present` + `ensure_edges_only`;
both gates check both matviews and cheaply repair a missing edges matview (existence dominates the
hash gate; non-fatal on the startup path); edges is built **before** the HNSW so the window
collapses. Self-heals on restart. Regression test: `memory_unified_edges_self_heal.rs`.

---

## 7. Bottom line

The Crucible tape work is high-craft but was shipped **inert**; it is now genuinely wired and every
CRITICAL/HIGH defect is fixed. The experiment→tracker integration was a real **self-verification
loophole** and was fully reverted (CI-only `→verified` restored). P9 and the experiment-API
improvements are sensible and were kept/built with the tracker firewall and anti-cherry-pick
guardrails intact. No agent can manipulate tracker verification state to inflate success.
