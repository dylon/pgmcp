# ADR-032 — Native in-process formal-verification tools

**Status:** Accepted · **Date:** 2026-06-21 · Builds on ADR-012 (the FV gate) and the
`lling-llang` symbolic-core hoist (Task #21).

## Context

ADR-012 established a formal-verification gate that drove *generation-time* planning and
review, but its discharge of universal protocol obligations (`NoStuck`, `Liveness`)
relied on **out-of-process** model checkers (pi + TLC). That has three costs: a process
boundary on every check, no first-class pgmcp tool surface for verification, and a
verdict that the agent cannot compose with the rest of pgmcp's in-process analysis
(effects, CSM, concurrency).

Task #21 hoisted a reusable, Rocq-backed **symbolic-automata + algebra-tower core** into
`lling-llang` (`lling_llang::symbolic`): effective Boolean algebras, SFA/SFT, the `Sat3`
Heyting/RejectSafe tower, `ConstraintTheory`/`TheoryAlgebra`, behavioral algebra, KAT,
subtype-lattice and Presburger decision procedures. pgmcp already depends on
`lling-llang`. This ADR records the decision to expose formal verification as **native,
in-process MCP tools** built on that core.

## Decision

Add a `router_fv` MCP tool family — six tools, each `{pgmcp data | inline spec} →
lling-llang/CSM engine → verdict`, with **no subprocess and no `prattail` dependency**:

| Tool | Engine | What it decides |
|------|--------|-----------------|
| `protocol_soundness` | CSM `mpst::well_formed` + the §4-D Rocq lemma | deadlock-freedom + progress for a `GlobalType` |
| `language_inclusion` | `SymbolicAutomaton::{intersect,complement,is_empty}` | `L(impl) ⊆ L(spec)` (the merge-coordinator feature-preservation primitive) |
| `presburger_decide` | `presburger::is_satisfiable_nfa` | Presburger-arithmetic satisfiability |
| `effect_verify` | `sema_helpers::effects` | effect-policy conformance `reachable ⊆ allowed` |
| `behavioral_check` | CTL fixpoint labelling (Clarke–Emerson–Sistla) | a CTL formula over a finite Kripke structure |
| `kat_hoare_check` | hoisted `kat_algebra::{BooleanTest, eval_test_public}` | propositional Hoare triple `{p}·c·{q} ≡ (p·c·¬q = 0)` |

Supporting components landed in the same task:

- **§4-B — SMT `ConstraintTheory` backend** (`lling_llang::symbolic::logict_smt`): an
  `impl ConstraintTheory for Z3Theory` makes `TheoryAlgebra<Z3Theory>` a `BooleanAlgebra`,
  so every SFA algorithm decides SMT-theory guards (bool / linear int / bitvectors). The
  Z3 **library** is dynamically linked in-process; solver `Unknown` is routed through
  `Sat3::DontKnow` ("possibly-sat", never a fabricated witness) — the `algebra_tower`'s
  three-valued logic is the soundness mechanism.
- **§4-C.1 — `SymbolicConstrainedDecoder`** (`lling_llang::llm::symbolic_decoder`):
  materializes a state's guard predicates into a per-state `TokenMask` at build time →
  O(1) logit mask at decode (never a live SAT call in the hot loop).
- **§4-C.2 — the `crucible-wfst` sidecar** (a new standalone crate): serves
  `/health /validate /constraint /repair /mask` over `lling_llang::symbolic` SFAs;
  `/mask` hosts the decoder. It is *outside* the pgmcp build by design (the CLI
  certificate path — cvc5/Z3 `--produce-proofs` — belongs to a subprocess host).
- **§4-D — `docs/formal/rocq/CsmDeadlockFreedom.v`**: the proofs-as-plans certificate —
  a well-formed `GlobalType` is deadlock-free and has progress *by typing* (admission-free
  under Rocq 9.1), demoting the universal `NoStuck` model-check to a redundant cross-check.

### The boundary rule (load-bearing)

The placement of each piece follows one rule: **a library link is in-boundary
(in-process); a CLI invocation is a subprocess and stays at the sidecar/pi edge.**

- `prattail` is pure Rust, but it is the user's *actively-refactored* crate; the symbolic
  engine it needs was hoisted into `lling-llang`, so **pgmcp links `lling-llang`, never
  `prattail`**. `protocol_soundness` therefore uses MPST well-formedness + the Rocq
  certificate rather than a WPDS prestar engine.
- The **Z3 library** is in-process (in-boundary). The **cvc5 / Z3 CLI** (`--produce-proofs`
  → Alethe/LFSC certificates) is a subprocess and lives in the sidecar.

## Consequences

- Verification verdicts compose in-process with pgmcp's effect, CSM, and concurrency
  analyses; the merge-coordinator's "no feature lost" check is now a native
  `language_inclusion` call.
- `protocol_soundness` closes the ADR-012 gap that needed pi + TLC — entirely in-process,
  machine-checked by `CsmDeadlockFreedom.v`.
- New transitive dependency: the `z3` crate (dynamically linked against the system libz3),
  added to `lling-llang` and thus to pgmcp. `z3_available()` guards runtime init so a
  solver-less host still links and runs. `verify.sh` Gate 2 covers the link.
- The sidecar is out-of-workspace and carries its own build/CI; it is not part of
  `verify.sh`.
- Soundness posture: SMT `Unknown` is conservative (`DontKnow`), `language_inclusion`
  reports a falsifiable witness word, `kat_hoare_check` a counterexample state, and
  `effect_verify` the shortest violating depth — every tool yields a falsifiable
  counterexample on failure (ADR-012 `Disc(P,M)=∅ ⇒ REJECT` alignment).
