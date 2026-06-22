# ADR-009: Modeling A2A multi-agent coordination as communicating state machines (CFSM + MPST), unified with RecursiveMAS

- **Status:** Proposed (Phase-1 deliverable = this ADR + the `src/csm/` skeleton; Tracks A/B build on it)
- **Date:** 2026-05-27
- **Related:** ADR-003 (closed-enum/tag-set discipline), ADR-004 (work-item tracker — the
  in-repo *guarded transition matrix* precedent, `src/tracker/transition.rs`), ADR-006 (adjacent
  tagging for recursive serde enums — **load-bearing here**), ADR-008 (temporal graph; bitemporal
  protocol nodes), the A2A/RLM subsystem (`src/a2a/`), the memory-server Phase 11 latent-link work
  (`src/llm/`), and `docs/formal/` (the TLA⁺/Rocq precedent: `CronStateMachine.tla`,
  `TransducerMandateDedup.v`).
- **Design plan:** `~/.claude/plans/we-have-modeled-the-compiled-ritchie.md`
- **Pedagogical treatise:** [`docs/csm/`](../csm/README.md) — the from-scratch design record
  (theory → data model → conformance → patterns → RLM → crucible execution), with diagrams,
  literate pseudocode, and links to the proofs.

## Context

pgmcp already reified RLM (`src/a2a/rlm.rs`) — recursive self-calls bounded by a depth/budget frame
— into a first-class, learnable, verifiable construct. The natural next move is to do the same for
*multi-agent coordination*. The decisive observation: **pgmcp already models state machines three
ways, and the `a2a_pattern_*` coordination topologies are the one place it does not.**

| Where | What it is | Form |
|───────|────────────|──────|
| `src/tracker/transition.rs` | 10-state work-item lifecycle, `check_transition()` with actor + evidence guards (ADR-004) | **Explicit guarded transition matrix** — a CFSM-with-guards in all but name |
| `src/cron/scheduler/state.rs` + `docs/formal/tla/CronStateMachine.tla` | 5-state reactive scheduler FSM | **FSM with a TLC-model-checked TLA⁺ spec** |
| `src/a2a/types.rs` `TaskState` | 6-state task lifecycle (`submitted→working→…→completed/canceled/failed`) | Implicit — the dispatcher writes any state, unguarded |
| `a2a_pattern_*` (sequential / mixture / distillation / deliberation / recursive) | multi-agent coordination topologies | **Hardcoded async/await loops + string sentinels (`"CONVERGED"`)** — not a transition system |

The coordination topology (who talks to whom, when to stop) is therefore invisible to inspection,
model-checking, replay-as-FSM, and the MSM trajectory learner. This ADR reifies it as an explicit
**Communicating-FSM network specified by Multiparty Session Types** — checkable (TLA⁺/Rocq),
replayable, and fed into the existing learning loop.

**There is abundant research.** Communicating FSMs (Brand & Zafiropulo, *JACM* 1983); Multiparty
Session Types (Honda/Yoshida/Carbone, POPL 2008; Scalas & Yoshida, POPL 2019); I/O Automata (Lynch &
Tuttle 1987); Statecharts (Harel 1987); process calculi CCS/CSP/π and rho-calculus/Rholang; MCMAS +
ATL and TLA⁺ for verification; and the 2024–26 LLM-agent wave — StateFlow, MetaAgent, LangGraph,
Agentproof, AgentGuard, PAT-Agent — plus L\*/Angluin automata learning and stochastic games.

**The RecursiveMAS connection (verified, not greenfield).** pgmcp's five `a2a_pattern_*` tools *are*
the four collaboration patterns of RecursiveMAS (Yang et al., *Recursive Multi-Agent Systems*,
arXiv:2604.25917) — Sequential (Planner/Critic/Solver), Mixture (Math/Code/Science + Summarizer),
Distillation (Expert/Learner), Deliberation (Reflector/Tool-Caller); `src/a2a/dispatcher.rs`
implements the paper's text-recursion baseline via `recursion_rounds`. Moreover, pgmcp already built
the **inner half of the paper's latent mechanism** under the memory-server's Phase 11 —
`src/llm/recursive_link.rs:1-4` implements the inner RecursiveLink `R_in(h)=h+W₂·σ(W₁·h)` and cites
*arXiv:2604.25917 §3.2* by name; with `src/llm/{latent_pipeline,qwen3_latent_model,latent_train}.rs`.
RMAS extends single-model recursion (RLM) to *system-level* recursion; the CFSM/MPST protocol is the
loop's skeleton and RecursiveLink is its latent channel.

## The load-bearing decision: coordination is a typed, projectable protocol

Adopt **Communicating Finite State Machines (CFSM)** as the operational model and **Multiparty
Session Types (MPST)** as the typing/projection discipline. Each coordination pattern becomes a
**global protocol type** `G`; each participant role `r` gets a **local type** `L_r = G ↾ r` by
projection; each agent at runtime is a CFSM whose alphabet is send/receive of typed messages. The
crux rules (mirroring ADR-004's structural-trust framing):

1. **The global type is the single source of truth** for a pattern's control flow. `project(G, r)`
   derives each role's local machine; `compile(project(G, r))` linearizes it to dense states. An
   *observer* lifts each real A2A run into a trace and `check_conformance(network, trace)` is the
   single chokepoint deciding whether the run conformed — the per-conversation analogue of
   `check_transition`'s per-item gate.
2. **Safety is a theorem, not a convention.** A well-formed `G` that projects yields local machines
   whose composition is **deadlock-free, orphan-free, and protocol-respecting** (the MPST
   metatheorem). Track A Phase 5 proves this in Rocq (general, ∀ well-formed `G`, to `Qed`); Phase 4
   model-checks the finite instances in TLC and proves the invariants ∀ model-size in TLAPS.
3. **Recursive AST ⇒ adjacent serde tagging.** `GlobalType`/`LocalType` are recursive (`Rec`/`Var`).
   Per ADR-006 they **must** use `#[serde(tag="type", content="data")]`; internal tagging stalls
   rustc ~2h. Non-negotiable; the Phase-1 round-trip test is the canary.
4. **Black-box roles are Text-only — a type law.** A role bound to a black-box agent (no hidden-state
   access: Claude Code, Codex) may appear only on `Text`-medium edges. A black-box role on a `Latent`
   edge is a `ProjectionError`, caught before any agent runs. "You cannot put Claude in the latent
   loop" is thus enforced structurally (Track B Tier-1), exactly as ADR-004 made "an agent cannot
   self-verify" a structural impossibility rather than a guideline.

**Why CFSM + MPST fits A2A best:** (i) the substrate *is* message-passing over channels, which CFSM
canonically models; (ii) MPST gives projection + global safety as theorems (the multiparty
generalization of what ADR-004 hand-proved with a matrix + property tests); (iii) it *subsumes*
rather than replaces — `TaskState` is the leaf CFSM each agent runs, the tracker matrix is a guarded
CFSM, a pattern is the composition; (iv) it is checkable with the house TLA⁺/Rocq toolchain; (v)
runs are trajectories, which already drive the MSM learner — so reification feeds the existing loop
with no new learner. RLM is the first reified protocol (`μX.G`); this generalizes it to N participants.

### Anchor decisions

- **Channel discipline:** synchronous (rendezvous) as the faithful primary model (A2A `tasks/send`
  blocks until the child task is terminal); a boolean `Async` bounded-FIFO variant models the
  genuinely-concurrent RLM fan-out and checks no-reordering. Both are model-checked.
- **Projection merge:** plain (syntactic) merge at recursion joins **plus** the restricted
  sender-driven *external-choice* merge — two receives from the *same* sender with distinct labels
  combine into a single `Branch` (the standard MPST projection rule that makes a choice's *bystander*
  projectable; e.g. the Tool-Caller in Deliberation). Anything else surfaces
  `ProjectionError::Unmergeable` — never a silent branch pick. The full subtyping lattice is **not**
  built (YAGNI for the fixed patterns).
- **Reification:** *observer first, interpreter later.* The five pattern tools stay byte-for-byte
  intact; a read-only conformance monitor lifts their existing transcripts. The interpreter (driving
  patterns *from* the protocol) is a later, flagged `PatternDriver` trait — never a cfg (the project
  has no `[features]`), built only after the observer shows 100% conformance.

## RecursiveMAS unification (Track B)

RMAS is the *latent channel medium* riding on the Track-A protocols. The loop A₁→A₂→…→Aₙ→A₁ run for
n rounds is the bounded recursive global type `μX.(A₁→…→Aₙ→A₁).X` — *the same skeleton* as a text
MAS; only label media differ (`MessageMedium::{Text, Latent}`), so the observer/TLA⁺/Rocq stack is
medium-agnostic. Three tiers: **Tier 1** (spec) the medium attribute + the black-box-Text-only
projection law; **Tier 2** (runtime, no GPU, black-box-friendly) a pattern-agnostic text-recursion
wrapper generalizing the ad-hoc `recursion_rounds` — the paper's own Recursive-TextMAS baseline;
**Tier 3** (extension, white-box) the outer link `R_out = W₃·h+W₂·σ(W₁·h)` + multi-role latent loop
+ inner-outer training, on the existing `src/llm/` candle foundation. The 8 GB VRAM ceiling makes
N≥2 heterogeneous 8B backbones infeasible locally, so Tier-3 v1 is a *homogeneous* Qwen3-8B loop
(`W₃=I`, per-role link + prompt swaps); the cross-architecture claim is validated via cloud-burst.

**As-built (R0–R4 shipped).** Track B is fully realized in `src/rmas/`:
- **R0** `src/a2a/recursion.rs` — the Tier-2 pattern-agnostic text-recursion wrapper (`CarryPolicy`,
  `converge_marker`), black-box-friendly, no GPU.
- **R1** `MessageMedium::{Text, Latent}` on the CFSM `Label` + the black-box-Text-only projection law
  (`csm::media`) + `RmasRecursionLoop.tla` + the Rocq medium-discipline lemma.
- **R2** `outer_link::OuterLink` (`R_out`, the cross-dim `W₃`) + `link_registry` + the `train-link` CLI.
- **R3** `loop_runner::HomogeneousQwen3Engine` — one resident backbone, per-role inner links, the
  `RmasEngine` trait + `make_engine` factory (degrades to `None` ⇒ Tier-2 text path, mirroring
  `make_latent_pipeline`), `residency` VRAM pre-flight, `patterns` (the 4 patterns → topology); run via
  `pgmcp rmas-loop`.
- **R4** `hetero_loop::HeterogeneousQwen3Engine` — multiple resident backbones, cross-dim outer-link
  ring hops; `train_outer` (the cross-dim `R_out` trainer, text-supervised since Q4 through-backbone
  autograd is blocked) + the `train-outer-link` CLI; run via `pgmcp rmas-loop --backbones 4b,8b,…`.

**Corrected feasibility finding (R3).** The plan estimated ~50 MB per per-role link; the accurate
`R_in` is two `hidden×hidden` F32 matrices — `2·4096²·4 ≈ 134 MB` for 8B (`≈ 52 MB` for 4B). At the
conservative 15% safety headroom the residency gate enforces, an 8B backbone (~6.55 GB) therefore
admits only ~1–2 resident roles on a fully-free 8 GB card; the 4B backbone (~2.6 GB) admits the full
multi-role loop. A genuine cross-architecture loop (4B@2560 + 8B@4096 ≈ 9.15 GB) exceeds the 8 GB
wall and runs only on a bigger GPU / cloud — the gate refuses it locally (`make_engine ⇒ None`)
rather than OOM mid-load. This sharpens, but does not change, plan risk 6: the homogeneous 4B loop is
the comfortable local target; 8B is single-/dual-role; heterogeneous is cloud.

## Alternatives considered

LTS is the shared semantics under all of these; they compose with the anchor rather than strictly
competing. Each non-chosen formalism, what it would be in pgmcp, and the trigger to add it:

| Formalism | What it would be | Verification shift | Upside | Cost | Add/switch trigger |
|───────────|──────────────────|────────────────────|────────|──────|────────────────────|
| **(a) Process-calculus LTS / rho-calculus** | Patterns as π/rho **processes**; agents as named channels; semantics = LTS; option to **emit Rholang** and run on a real rho-VM | **Bisimulation** instead of TLC reachability | Max expressiveness (name mobility, dynamic topology); pgmcp already ships the Rholang/π pattern catalog (`src/patterns/process_calculus.rs`); a real LTS substrate exists (F1r3fly/RChain) | No turnkey deadlock-freedom theorem; bisimilarity costly/undecidable in general | Patterns reconfigure topology at runtime, **or** the user wants A2A protocols to *execute* on the rho-VM → then ADD: MPST = the contract, Rholang = the certified runtime |
| **(b) Statecharts (hierarchical/concurrent)** | Each agent = a Harel statechart (nested + AND-parallel + history) | TLA⁺ still applies (flatten); no native multiparty-safety theorem | Compact encoding of RLM depth-nesting + `InputRequired`/resume; SCXML tooling | Single-machine/broadcast bias — wrong shape for *distributed* point-to-point channels | A *single* agent's internals get complex → ADD statecharts *inside* a CFSM node, not at the coordination layer |
| **(c) Markov / learned automata (stochastic games + L\*)** | Pattern = stochastic game/MDP; peer FSMs **inferred** from observed I/O traces via L\*/Angluin | **PAC-learning** convergence + ATL/MCMAS, instead of a-priori model checking | Coordinate with peers whose protocol you don't know/control; complements the trajectory learner | LLM nondeterminism breaks classical exact L\* (needs probabilistic/Mealy/MDP); equivalence queries unrealizable against a live peer | Must coordinate with an unknown/untrusted external peer, **or** the goal shifts from *verifying* to *optimizing* policy → ADD as an observer-side module (this ADR's Phase 8 takes the L\* slice) |

**Rejected as the primary anchor:** (a) is the richest and most aligned with the user's Rholang work
but carries no off-the-shelf deadlock-freedom theorem and exceeds the fixed patterns' needs today —
it is retained as the *execution-substrate* forward option, not the typing layer; (b) models one
machine with broadcast events, the wrong shape for distributed point-to-point A2A; (c) is
probabilistic and observer-side, folded in as Phase 8 rather than the spine.

## Consequences

- New top-level module `src/csm/` (CFSM core + `csm/mpst/` AST/projection/well-formedness),
  closed-enum + pure-total-`check_step` discipline copied from `src/tracker/transition.rs`.
- A v7 migration (`csm_protocols`/`csm_projections`/`csm_run_traces`) and `csm_*` MCP tools
  (introspection + the keystone observer `csm_validate_run`); resources + completions; the
  interpreter swapped behind `[a2a] protocol_interpreter` later.
- Formal artifacts under `docs/formal/` (5 TLA⁺ instances + a generic core; 7 Rocq `.v` files proving
  T1–T6 to `Qed`); `tlapm` becomes a build prerequisite for the TLAPS gate.
- Track B: `src/a2a/recursion.rs` (text-recursion wrapper), `MessageMedium` on the CFSM `Label`, and
  `src/rmas/` (outer link + latent loop) on the existing `src/llm/` foundation.
- The CUDA-mandatory / no-cargo-features / idempotent-migrations / `.expect`-over-`unwrap` /
  preallocation constraints carry over unchanged.

## When to reconsider

If A2A coordination stays at ~5 fixed, in-process, trusted-peer, text-only patterns forever, the full
MPST machinery may exceed the need — say so honestly. The payoff arrives with (a) more or
runtime-dynamic patterns, (b) untrusted/unknown external peers (Phase 8 / alternative (c)), (c) a
demand for mechanical deadlock-freedom guarantees, or (d) the latent-efficiency regime (Track B,
which needs white-box models). Phase 1 buys optionality + a checked model of one real pattern
(Deliberation, the only one with a genuine choice/merge) at low cost; later phases each rest on a
concrete dependency, not speculation.
