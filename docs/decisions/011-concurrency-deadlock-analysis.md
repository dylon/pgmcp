# ADR-011: Static deadlock detection & concurrency-bottleneck analysis (lock-order graph + channel Petri net), with TLA⁺/Rocq soundness

**Status:** Accepted · **Date:** 2026-06-01
**Related:** ADR-009 (CFSM/MPST — the formally-backed-ADR template this pairs TLA⁺+Rocq+impl after, but for *agent protocols*, not code concurrency), ADR-003 (closed-vocabulary tag sets — the `sync_ops` enums + `ConcurrencyFindingKind` follow it), ADR-006 (adjacent serde tagging), the shadow-ASR semantic layer.

This ADR is to *code-level concurrency* what ADR-009 was to *agent-protocol* coordination: an explicit, formally-backed model replacing convention.

## Context

The shadow-ASR semantic layer already extracts concurrency-relevant **effects** (`channel_send`, `channel_receive_linear/persistent`, `process_spawn`, `async`, `blocking_io`, `unsafe`, `may_panic`) and feeds the unified graph. But the existing concurrency MCP tools were **shallow, syntactic heuristics**:

- `deadlock_candidates` regex-matched `lock(A); lock(B)` *within a single function*, built a lock-order map from `acquires.windows(2)`, and ran Tarjan SCC — **no lock identity, no release tracking, no interprocedural reach**, so it could not catch the common `A→B→C→A` cycle spanning function boundaries.
- `lockset_races` was a regex audit-list, not a real Eraser lockset.
- There was **no channel / message-passing deadlock analysis at all** — a glaring gap given the corpus is F1R3FLY/Rholang-adjacent (rho-calculus channels with linear vs. persistent receive).
- No tool carried any *soundness* guarantee for the "no cycle ⇒ safe" claim its SCC test implied.

## Decision

A **best-per-paradigm portfolio**, grounded in an ordered synchronization skeleton, with full formal backing.

1. **Extraction (`sync_ops`, migration v21).** Per function/contract, an *ordered* skeleton of synchronization ops — lock acquire/release (with RAII release synthesis), channel send/recv (linear vs. persistent), spawn, await, select — with best-effort resource identity and a confidence tier. Rust (`syn`) + Rholang (tree-sitter) ship in v1; other languages keep coarse effect membership. A coarse mirror (`lock_acquire`, `lock_release`, `thread_spawn`, `await_point`, `channel_select` effects) flows into `symbol_effects` and the v15 effect-drift ledger for free.

2. **Shared-memory deadlock — interprocedural lock-order graph** (`src/graph/lock_order.rs`). Nodes = lock resources; edge `A→B` iff B is acquired while A is held, computed by a per-symbol held-set walk and **inlined across the resolved call graph** (the RacerD/Infer "deadlock domain": at a call site reached while holding A, every lock the callee may acquire within K hops is ordered after A). Deadlock candidate ⟺ a cycle (Tarjan SCC), with rwlock read/write refinement (an all-read cycle cannot deadlock) and resource-confidence-floored edges.

3. **Message-passing deadlock — channel Petri-net signals** (`src/graph/petri.rs`). Polynomial structural signals over per-process channel skeletons: `blocked_recv` (a linear receive whose channel has no producer), `orphan_send`, and `channel_cycle` (processes each *initially blocked* on a receive only another blocked process produces — a communication deadlock). Rholang persistent receives (`<=`) stay armed and are excluded from starvation.

4. **Bottlenecks** (`concurrency_bottlenecks`): lock contention, channel imbalance, spawn fan-out, and async stalls, ranked by `file_metrics.pagerank` (the `io_hotpath` centrality proxy).

5. **Tools:** `deadlock_cycles`, `channel_deadlock`, `concurrency_bottlenecks`, `lock_order_graph`, `sync_skeleton` (router `concurrency`). `deadlock_candidates` is kept as a zero-dependency regex pre-filter and its description points to `deadlock_cycles` as the deeper successor.

## Soundness / completeness posture (precise)

The method is **sound for what it claims and nothing more.**

- **Lock-order acyclicity is *sufficient*, not necessary, for deadlock-freedom.** A cycle is a *candidate*, not a proof. The analysis **flags candidates**; it never certifies a program deadlock-free except modulo the extracted graph.
- **Over-approximation → false positives possible.** The lock-order graph unions edges over all paths/contexts; a flagged cycle may be infeasible at runtime (guarded by disjoint conditions, gated by a higher lock, or on dead code). This is the *safe* direction for a bug-finder.
- **Under-approximation w.r.t. extraction → false negatives possible.** Locks taken via paths the call-graph resolver misses (dynamic dispatch, FFI, macros, calls below the confidence floor) produce missing edges. The proof's soundness is **conditional on the extracted relation faithfully containing the program's realized wait-for edges** (the `respects` premise in `LockOrderDeadlock.v`).
- **Channel side:** `blocked_recv` / `channel_cycle` are each *sufficient* for a stuck transition set (proven). Siphon-based detection is *not complete* for deadlocks in general (non-free-choice) nets — again candidates, not certificates.
- **What is mechanically guaranteed:** the *reduction* is sound — IF the extracted graph is acyclic THEN (modulo extraction faithfulness) no circular-wait deadlock exists; and IF a circular-wait state is reachable THEN the graph has a cycle (so the flag fires). The proofs close the gap between "Tarjan found an SCC" and "this is the right thing to compute."

## Formal-verification summary

All four artifacts are auto-discovered by `scripts/verify.sh` (per-file `coqc` / per-spec `tlc`; zero gate change) and were validated locally (Rocq 9.1.0, TLC 2.19).

**Rocq (`docs/formal/rocq/`)** — Stdlib-only, no `Admitted`/`Axiom`/`Hypothesis`:
- `LockOrderDeadlock.v` — the centerpiece. `wait_for_subset_lockorder` (the reduction: a held→requested pair *is* a lock-order edge) → `clos_trans_mono_into_Rplus` → `cycle_in_acyclic_is_false` → **`acyclic_implies_deadlock_free`** (∀ finite set of skeleton-respecting processes) + the contrapositive `deadlock_implies_lockorder_cycle` (the completeness direction the tool relies on).
- `ChannelDeadlock.v` — `needs_unmarked_siphon_not_enabled` → `unmatched_recv_blocked` (backs `blocked_recv`) + **`unmarked_siphon_dead`** / `cyclic_wait_deadlocks` (a dead marking — backs `channel_cycle`). Proves the dead-marking *sufficiency* the tool claims; full Commoner liveness (siphon stays empty forever) is explicitly out of scope (the tool reports a candidate, not a forever-stuck certificate).

**TLA⁺ (`docs/formal/tla/`)** — bounded TLC models confirming the operational semantics:
- `LockOrderDeadlock.tla` — 2 processes / 2 locks, rank-consistent (acyclic) acquisition order ⇒ `[]NoDeadlock`. The operational confirmation that ordered acquisition prevents circular wait (Havender).
- `ChannelDeadlock.tla` — 2 processes over bounded channels, producer-first ⇒ `[]NoDeadlock`.
- *Deadlock-reachability witnesses* (invert an order / make both processes receive-first) are documented in each spec header as **manual** runs: asserting `INVARIANT NoDeadlock` on the unsafe config makes TLC emit the circular-wait trace, which is a non-zero exit by design and therefore not part of the green gate. The completeness direction is instead covered formally by the Rocq contrapositive.

Cite: Coffman, Elphick & Shoshani (1971); Havender (1968); Commoner (1972); Murata (1989); Tarjan (1972).

## Alternatives rejected

1. **A single unified Petri net for both locks and channels.** Locks have a clean order-theoretic invariant (acyclicity ⇒ safety, a one-line `clos_trans` proof) that a Petri-net encoding obscures behind siphon analysis; modeling mutexes as 1-bounded places loses the Havender structure and makes the soundness proof far heavier. Two specialized models each get a short, complete proof; the union does not.
2. **Per-project model-checking (TLC/SPIN on the extracted model) as the primary guarantee.** Does not scale (state explosion at realistic lock/channel counts) and yields no general theorem — only "this project, this bound." TLC is kept for the small *witness* models; the soundness of the *method* is the Rocq theorem (∀ N / ∀ graph).
3. **Reusing CSM/MPST (`src/csm/`) for locks.** MPST deadlock-freedom is about *message-passing session* well-formedness (projection/merge of global types); it has no notion of a held-lock set or a resource-allocation wait-for graph, and its progress theorem is a *different* property. Forcing locks into the session-type frame would require encoding each lock as a two-party rendezvous and re-deriving Coffman from projection soundness — strictly more work and conceptually wrong. The channel analysis is *closer* to MPST but still distinct (we analyze an extracted net structurally, not type a protocol), so it too gets its own proof. We reuse CsmMpst.v's *style* and the verify.sh/ADR machinery — the methodological reuse, not the theorems.

## Consequences

- New `docs/formal/{rocq,tla}` artifacts, auto-gated by verify.sh (zero gate change); the formal README gains rows for them.
- The concurrency tools gain documented soundness backing (their `guidance` strings cite `docs/formal/rocq/*.v` and this ADR).
- Findings persist to the `concurrency_findings` ledger (v22) and integrate with the temporal graph-RAG (Layer 4): `lock_order_edges` become bitemporal unified-graph edges (`as_of`-queryable), and per-project health snapshots feed the forecast/trajectory machinery — reusing infrastructure rather than rebuilding it.
- The closed-vocabulary / idempotent-migration / `.expect()` constraints carry over.

## When to reconsider

If a *certified* "this program is deadlock-free" guarantee (not candidate flagging) is ever required, the extraction-faithfulness premise (`respects`) must itself be discharged against the real IR — a much larger verified-extraction effort. Dynamic lock hierarchies that change at runtime, or unbounded channels, break the finite/bounded modeling and would need a different (possibly coinductive) treatment.

## References

- `docs/formal/rocq/LockOrderDeadlock.v`, `docs/formal/rocq/ChannelDeadlock.v`
- `docs/formal/tla/LockOrderDeadlock.tla` (+`.cfg`), `docs/formal/tla/ChannelDeadlock.tla` (+`.cfg`)
- `src/graph/lock_order.rs`, `src/graph/petri.rs`, `src/concurrency/`, `src/parsing/sync_ops.rs`, `src/parsing/rust/sync_ops.rs`
- `src/mcp/tools/tool_{deadlock_cycles,channel_deadlock,concurrency_bottlenecks,lock_order_graph,sync_skeleton}.rs`
- Migrations: `src/db/migrations/v21_sync_ops.rs`, `src/db/migrations/v22_concurrency_findings.rs`
- Coffman/Elphick/Shoshani 1971; Havender 1968; Commoner 1972; Murata 1989; Tarjan 1972
