# ADR-030: Pushdown / hierarchical CSM — VPA + RSM + HSM unification

- **Status:** Accepted — **type layer, projection, well-formedness, RSM compiler,
  pushdown conformance engine, driver, and the hierarchy-preserving crucible
  synthesis are implemented and tested** (101 unit tests green). Mechanized
  proofs (Rocq `CsmPushdown.v`, TLA⁺ `PushdownCsm.tla`) and the `verify.sh` gate
  are the specified next step (§9).
- **Date:** 2026-06-21
- **Relates to:** ADR-009 (A2A coordination state machines — *refined* here, §8),
  ADR-028 (category-theoretic layer — `gseq`/projection laws reused, §8),
  ADR-003 (closed-vocabulary idiom — `StackAction`), ADR-006 (adjacent serde —
  the recursive AST). Modules: `src/csm/` (`mpst/{global,local,project,wellformed}`,
  `machine`, `transition`, `conformance`, `driver`, `registry`, `examples`,
  `role`), `src/mcp/tools/tool_csm_synthesize_protocol.rs`,
  `src/mcp/tools/tool_session_checkpoint_resume.rs`,
  `src/db/migrations/v54_csm_pushdown.rs`. Reuses the workspace's bounded
  pushdown primitive concept from `lling-llang::pushdown` / `context-tape`.

## Context

> *Can the CSM (Communicating State Machine, `src/csm/`) be used as a pushdown
> automaton / hierarchical state machine for complex tasks?*

The CSM was, until this change, **provably finite-state**: a network of
**Communicating Finite State Machines** (CFSM, Brand–Zafiropulo 1983) projected
from **Multiparty Session Types** (MPST, Honda–Yoshida–Carbone 2008). Concretely:

- a state is a flat integer (`machine.rs`: `pub type LocalState = usize`) — there
  are no composite/nested states;
- the only recursion, `Rec{var}` / `Var{var}`, compiles to **back-edge self-loops**
  into already-allocated states (`machine.rs`) — that is regular (Kleene-star),
  *not* context-free;
- conformance was finite-state replay — one current state per role, **no stack**.

The **primary use case** that motivates lifting this limitation is the **crucible**
crate of agents, whose Planner produces **deeply hierarchical plans**. Those plans
live as a work-item tree (`work_items.parent_id`/`root_id`), but the single bridge
to an executable protocol — `csm_synthesize_protocol` — **flattened the tree into a
single-level chain**, discarding the nesting. A finite-state contract cannot
express "run this sub-plan to completion, then return to the parent step" without
unbounded unrolling; a Planner's nesting was therefore *erased* at the protocol
boundary.

The terms used below, defined before use:

| Symbol / term | Definition |
|---|---|
| **DFA / FSM** | Deterministic finite automaton; recognizes the **regular** languages. |
| **PDA** | Pushdown automaton = FSM + an unbounded LIFO **stack**; recognizes the **context-free** languages (CFL). |
| **DPDA** | Deterministic PDA; recognizes the deterministic CFLs (DCFL). |
| **VPA** | *Visibly* pushdown automaton (Alur–Madhusudan 2004): a PDA whose stack action (push/pop/none) is fixed by the **input symbol's class** (call/return/internal), not the state. Recognizes the **visibly pushdown languages** (VPL). |
| **RSM** | Recursive State Machine (Alur et al. 2005): a finite set of component machines ("boxes") that may **call** one another (with return); equivalent to pushdown systems. |
| **HSM** | Hierarchical State Machine (Harel statecharts 1987): states may contain nested sub-states (composite states). |
| **well-nested / Dyck-balanced** | A run whose calls and returns match like balanced brackets — every call is closed by exactly one later return. |
| `Σ_c, Σ_r, Σ_int` | The call / return / internal alphabet partition of a VPA. |

The strict containment that frames the design (each `⊊` is proper):

```
            ┌────────────────────────── CFL (context-free) ──────────────────────────┐
            │   ┌──────────────────── DCFL (deterministic CF) ───────────────────┐   │
            │   │   ┌──────────────── VPL (visibly pushdown) ───────────────┐    │   │
            │   │   │   ┌──────────── regular (FSM / the old CSM) ───────┐   │    │   │
            │   │   │   │  Sequential · Mixture · Distillation ·         │   │    │   │
            │   │   │   │  Deliberation · WorktreeNegotiation · TapePaging│   │    │   │
            │   │   │   └────────────────────────────────────────────────┘   │    │   │
            │   │   │      ▲  RecursiveCf (genuine recursion) ·  HSM boxes ────┘    │   │
            │   │   │         crucible nested plans  (THIS ADR)                     │   │
            │   │   └───────────────────────────────────────────────────────────┘   │
            │   └───────────────────────────────────────────────────────────────────┘
            └───────────────────────────────────────────────────────────────────────┘
```

`regular ⊊ VPL ⊊ DCFL ⊆ CFL`. The old CSM sat in the innermost band; this ADR
moves the recursive/hierarchical protocols out to **VPL** — far enough to express
unbounded matched call/return nesting, but no further, because VPL keeps the two
properties finite-state lost yet a general PDA does **not** retain: **closure
under ∩/∪/¬** and **decidable inclusion**, which is exactly what a *conformance
checker* needs (Alur–Madhusudan 2004).

## Decision

Adopt a **unified VPA + RSM + HSM** model with **VPA as the decidable conformance
core** and **RSM/HSM as the structuring layer that compiles down to it**:

1. **VPA** — partition the protocol alphabet into call (push) / return (pop) /
   internal (nop); a conforming run is well-nested.
2. **RSM** — a `GlobalCall` to a *named, closed* sub-protocol pushes a frame; the
   callee's `End` pops it and resumes the caller's continuation. A protocol may
   name **itself** → unbounded nesting from finite syntax (`RecursiveCf`).
3. **HSM** — composite/hierarchical states are realized **as RSM boxes**: an
   inline `GlobalBox` is a sub-region entered on `enter` (push) and exited on
   `exit` (pop). HSM thus *falls out of* RSM with no separate machinery — the
   nesting a crucible plan tree carries becomes nested boxes.

### The AST (adjacent-tagged per ADR-006), `src/csm/mpst/global.rs`

```
                       GlobalType  (before → after this ADR)
   ┌─────────────────────────────┐        ┌─────────────────────────────────────┐
   │ Interaction{from,to,label,…}│        │ Interaction{from,to,label,cont}      │
   │ Choice{from,to,branches}    │        │ Choice{from,to,branches}             │
   │ Rec{var,body}   ── loop ──▶ │   ⟹    │ Rec{var,body}     (regular back-edge)│
   │ Var{var}                    │        │ Var{var}                             │
   │ End                         │        │ GlobalCall{callee,subst,cont} ◀ NEW  │  push/pop (RSM)
   └─────────────────────────────┘        │ GlobalBox{enter,body,exit,cont} ◀ NEW│  push/pop (HSM box)
                                          │ End                                  │
                                          └─────────────────────────────────────┘
```

`Rec`/`Var` are **kept verbatim** as the regular self-loop; `GlobalCall` and
`GlobalBox` are the *only* new context-free constructs. The discriminating field is
`cont`, the **return continuation** — precisely what a tail `Rec`/`Var` back-edge
cannot express (you cannot "loop back *and* continue afterwards" without a stack).
`callee: ProtocolRef` is a *name* resolved through a `ProtocolEnv`
(`registry::protocol_env`), so a protocol can reference itself; `subst:
BTreeMap<Role,Role>` renames the callee's roles into the caller's space (injective,
checked by WF-THREAD). `LocalType` mirrors with `LocalCall` / `LocalBox`.

### The visibly-pushdown alphabet (`StackAction`, `role.rs`)

`StackAction { Neutral, Push, Pop }` (closed vocabulary, ADR-003 idiom — golden
test pins `'neutral','push','pop'`). `GlobalType::alphabet()` classifies each
symbol *structurally*: `Interaction`/`Choice` labels are `Neutral`; a `GlobalCall`
contributes `call:<callee>` (Push) and `ret:<callee>` (Pop); a `GlobalBox`
contributes its `enter` (Push) and `exit` (Pop). **WF-VPA** (`wellformed.rs`)
enforces the visibly-pushdown invariant: each label name maps to exactly one stack
action, and no ordinary label squats the reserved `call:`/`ret:` prefix — this is
what guarantees the recognized class is a VPL (not a general PDA).

### Projection — the participation rule (`project.rs`)

The load-bearing reconciliation: a role that **participates** in a frame (it is in
`subst`'s image, or in a box body's participants) projects to a `LocalCall`/
`LocalBox` and **pushes/pops synchronously** with the other participants; a
**bystander skips the entire closed frame** (projecting only the continuation).
WF-THREAD guarantees a frame's role-set is fixed across choice branches, so a
bystander is never "in the call in one branch and out in another" — keeping the MPST
`merge` total and projection sound **without any global broadcast**. (This is *why
VPA, not general PDA*: the stack action is visible per-participant.)

### Compiler — RSM boxes (`machine.rs`)

`compile_in(role, lt, env)` produces a `LocalMachine` over a single global state
space with `EdgeKind { Internal, Call{return_state}, Return }`: a `Call` edge
pushes the call site's return address and enters the callee/box; the callee/box
`End` becomes a `Return` (pop). A recursive callee is compiled **once** per
`(callee, callee-role)` and reused — the RSM back-edge that yields unbounded
nesting from finite boxes (Alur et al. 2005). **Call-free protocols compile to
`Internal`-only edges, byte-identical to the pre-ADR CSM** (a golden test asserts
the 7 legacy protocols are unchanged).

### Conformance — bounded pushdown replay (`conformance.rs`)

```
            per-role pushdown configuration:  (state, stack: Vec<LocalState>)
   event (from →label→ to):
       sender   : ε-close ─▶ Internal edge on Send(to,label) ─▶ ε-close
       receiver : ε-close ─▶ Internal edge on Recv(from,label) ─▶ ε-close
   ε-close(state, &mut stack):                         ┌── Call{ret} ▶ push ret ; state:=callee_entry
       follow Call/Return boundary edges ──────────────┤
       (bounded by MAX_STACK_DEPTH)                     └── Return    ▶ ret:=pop()? ; state:=ret
   accept  ⇔  ∀ roles:  terminal(state)  ∧  stack empty        (well-nested)
```

`Call`/`Return` are **ε-structural** (taken during ε-closure, not consumed from the
trace); a non-empty residual stack ⇒ `Unbalanced`; a push past `MAX_STACK_DEPTH` ⇒
`DepthExceeded`. For a call-free protocol every stack stays empty and the replay
coincides exactly with the pre-ADR finite-state replay (so all legacy conformance
tests pass unchanged). `replay_to_configs` returns the per-role `(state, stack)` —
the **stack-aware resume position** ("the *stack of frames* IS the position",
generalizing ADR-009's "the trace is the position"); `session_checkpoint_resume`
surfaces the recovered `frame_depth`.

### Tested evidence (the operational answer = **YES**)

`recursive_cf` (the genuine self-calling pushdown protocol) **conforms at depths
2–5** — unbounded matched call/return nesting that a finite-state CSM provably
cannot recognize — and **rejects** non-well-nested / over-deep / out-of-order runs;
HSM `GlobalBox` runs conform and a body-skip is rejected; the hierarchy-preserving
`csm_synthesize_protocol` turns a `root → phase → {a,b}` plan tree into a
nested-box protocol that validates, projects, compiles to real `Call` edges, and
conforms. 101 unit tests green.

## Two corrections to the original plan (recorded for honesty)

1. **Conformance engine: a direct per-role stack replay, not `lling-llang::pushdown::
   PdaDecoder`.** That engine matches each transition against a *concrete* stack
   top, so a stack-neutral internal move needs one transition per stack symbol —
   and an RSM's stack alphabet is the set of per-call-site return addresses, so
   reusing it would blow up multiplicatively (it is built for grammar decoding with
   a tiny `Z₀`/paren alphabet). A `Vec<LocalState>` push/pop over the compiled
   `Call`/`Return` edges is exact, has no blowup, and is simpler. Its **ε-closure
   insight** (call/return as ε-moves, real communications as input) is retained.
2. **The RLM runtime depth cap stays small (4); only the *static* conformance bound
   is large (`MAX_STACK_DEPTH = 4096`).** Equating them — as a first draft proposed
   — would be a DoS vector: each RLM level issues real LM sub-calls, so its depth is
   a cost bound that must stay low, whereas the conformance bound governs *cheap
   static* trace-checking where deep nesting costs nothing. The two are documented
   as deliberately distinct in `rlm.rs`.

## Crucible application (the primary use case)

`csm_synthesize_protocol` is now **hierarchy-preserving**: the Planner's work-item
tree is reconstructed and folded so a leaf becomes a worker request/response and an
**interior item becomes a nested `GlobalBox` composite state**, sequenced via the
proven `GlobalType::then` (ADR-028 CT-2 monoid). A crucible plan's nesting is thus
carried into a genuinely hierarchical (pushdown) protocol instead of being
flattened — and the result is conformance-checkable and pause/resume-recoverable
*at depth*. (The finite work-item tree is bounded, so it uses inline HSM boxes;
unbounded *runtime* decomposition is the separate `RecursiveCf`/RLM path.)

## Formal-verification architecture (§9 — specified, next step)

Mechanized verification extends the existing artifacts; the design is fixed so the
proofs are a transcription task, not a discovery one. **Bounded stack is the
linchpin**: a `< D` push guard makes the reachable-configuration set finite, which
keeps the Rocq model an ordinary `Inductive` (no coinduction) *and* gives TLC a
finite model — "coinduction is the price of unbounded behavior; a configurable
bound buys it back."

```
   bound D  ─▶  finite reachable configs  ─┬─▶  Rocq: well-founded nat measure (no cofix)
                                           └─▶  TLC: finite model (MaxStackDepth = 2)
```

- **Rocq** — edit `docs/formal/rocq/CsmMpst.v` to add `GRec/GVar/GCall` (de Bruijn)
  and re-prove T3–T6 + CT-1/CT-2 over the enlarged type; add a self-contained
  `CsmPushdown.v` (the harness `coqc`s each file standalone) proving, with **no
  axioms/admits**: `config_wf_preserved` (subject reduction preserves
  well-nesting), `reachable_well_nested` (every reachable config has a balanced,
  ≤ `D` stack), `pushdown_terminates` (lexicographic `(D − depth, size)` measure,
  re-deriving the existing `rlm_terminates` as the single-recursion corollary), and
  `conformance_sound`/`conformance_complete` (a run is accepted iff it is a
  well-nested trace of the protocol — the decidable VPA core).
- **TLA⁺** — `PushdownCsm.tla` + `VpaConformance.tla` with an explicit bounded
  `stack` Seq; invariants `TypeOK`/`StackBounded`/`WellNested`/`NoOrphan`;
  properties `DeadlockFreedom` (terminal = `End` ∧ empty stack) + `EventualTermination`;
  TLC `MaxStackDepth = 2` for finiteness (the runtime/Rocq bound is large — the ADR
  states this split). Keeps the `src/csm/mod.rs` golden topology test green.

## Reconciliation with prior ADRs

- **ADR-009** rejected Harel statecharts *at the coordination layer* ("a single
  agent's internals get complex → add statecharts inside a CFSM node, not at the
  coordination layer"). That was an **agent-authored** decision. ADR-030 *refines*
  it: hierarchy **is** adopted at the coordination layer — but only its
  recursive/nesting structure, recast as **RSM boxes over a VPA**, *not* Harel's
  broadcast-event / AND-parallel semantics. The original objection (broadcast bias,
  wrong shape for point-to-point A2A) still stands and is *avoided* precisely
  because we use point-to-point RSM call/return, not statechart broadcast. So:
  *hierarchy — yes; Harel's broadcast model — still no.*
- **ADR-028** — projection-as-functor (CT-1) and the `gseq`/`then` monoid (CT-2)
  are reused directly: `then` sequences sibling subtrees in the hierarchy-preserving
  fold, and projection now ranges over call/return reachability. The category story
  is *extended*, not redefined.

## Consequences

- **Positive.** Crucible's hierarchical plans are first-class pushdown protocols:
  conformance-checkable (well-nesting enforced), pause/resume-recoverable at depth,
  and statically analyzable while remaining decidable (VPL closure). The legacy
  finite-state protocols are byte-identical and untouched.
- **Cost / when to reconsider.** The stack is **bounded** (`MAX_STACK_DEPTH`), a
  real (large) limit surfaced as a `DepthExceeded` refusal, not silent truncation.
  Going to full CFL / true unbounded coinductive recursion would sacrifice the
  decidable conformance that is the whole point — reconsider only if a protocol
  genuinely needs unbounded *live* recursion that no large finite bound covers.
- **Trust boundary preserved.** The compiler adds stack guards, never a new
  role-conditioned edge; the no-`Agent`-arm-in-judgment-states property is intact.

## Citations (DOIs verified via Crossref)

1. R. Alur, P. Madhusudan. *Visibly Pushdown Languages.* STOC 2004.
   [`10.1145/1007352.1007390`](https://doi.org/10.1145/1007352.1007390)
2. R. Alur, M. Benedikt, K. Etessami, P. Godefroid, T. Reps, M. Yannakakis.
   *Analysis of Recursive State Machines.* ACM TOPLAS 2005.
   [`10.1145/1075382.1075387`](https://doi.org/10.1145/1075382.1075387)
3. D. Harel. *Statecharts: a visual formalism for complex systems.* Sci. Comput.
   Program. 1987. [`10.1016/0167-6423(87)90035-9`](https://doi.org/10.1016/0167-6423(87)90035-9)
4. K. Honda, N. Yoshida, M. Carbone. *Multiparty Asynchronous Session Types.*
   POPL 2008. [`10.1145/1328438.1328472`](https://doi.org/10.1145/1328438.1328472)
5. A. Das, H. DeYoung, A. Mordido, F. Pfenning. *Nested Session Types.* ESOP 2021.
   [`10.1007/978-3-030-72019-3_7`](https://doi.org/10.1007/978-3-030-72019-3_7)
6. P. Thiemann, V. T. Vasconcelos. *Context-Free Session Types.* ICFP 2016.
   [`10.1145/2951913.2951926`](https://doi.org/10.1145/2951913.2951926)
7. D. Brand, P. Zafiropulo. *On Communicating Finite-State Machines.* JACM 1983.
   [`10.1145/322374.322380`](https://doi.org/10.1145/322374.322380)
8. A. Scalas, N. Yoshida. *Less is More: Multiparty Session Types Revisited.*
   POPL 2019. [`10.1145/3290343`](https://doi.org/10.1145/3290343)
