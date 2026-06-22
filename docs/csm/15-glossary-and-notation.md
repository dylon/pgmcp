# 15 — Glossary & notation

> The consolidated, pedagogical reference: every symbol, acronym, and key term defined
> once, with a pointer to the chapter that treats it in full; the notation table; and the
> master bibliography.

**Builds on:** all chapters. Back to [README](README.md).

---

## 15.1 Glossary

### Theory & automata

| Term | Definition | Chapter |
|------|-----------|---------|
| **CSM** | Communicating State Machine — the network-of-CFSMs model of A2A coordination (the code's term; the user's "communicative state machine" is a synonym). | [00](00-motivation-and-overview.md) |
| **CFSM** | Communicating Finite-State Machine (Brand–Zafiropulo 1983): finite automata interacting by message-passing over channels. | [01](01-cfsm-mpst-foundations.md) |
| **MPST** | Multiparty Session Types (Honda–Yoshida–Carbone 2008): a typing discipline for message-passing protocols, with projection and a safety metatheorem. | [01](01-cfsm-mpst-foundations.md) |
| **global type** `G` | The bird's-eye protocol type — who sends what to whom, in what order, with what choices and recursion. | [01](01-cfsm-mpst-foundations.md) |
| **local type** `G ↾ r` | One role's view of the protocol, derived by projection. | [01](01-cfsm-mpst-foundations.md) |
| **projection** | The total recursive function `G ↾ r` deriving a role's local type (sender→`Send`, receiver→`Recv`, bystander→merge; participant→`Call`/`Box`). | [02](02-projection-and-wellformedness.md) |
| **merge** (`⊓`) | Reconciles a bystander's branch continuations; the *external-choice merge* combines two same-sender receives into one `Branch`. | [02](02-projection-and-wellformedness.md) |
| **well-formedness** | The eight static side-conditions (no self-message, closed, guarded recursion, sender-driven choice, matching-return box, WF-THREAD, WF-VPA, WF-CLOSED/BOUND) that make `G` projectable and safe. | [02](02-projection-and-wellformedness.md) |
| **participation rule** | A frame's participant pushes/pops synchronously; a bystander skips the closed frame — what keeps projection sound without broadcast (and why VPA, not general PDA). | [02](02-projection-and-wellformedness.md) |
| **DFA / PDA / DPDA / VPA** | Finite / pushdown / deterministic-pushdown / *visibly*-pushdown automata. | [04](04-automata-spine.md) |
| **VPL** | Visibly Pushdown Language: the class CSM targets — closed under `∩ ∪ ¬`, decidable inclusion, recognizing well-nested call/return. | [04](04-automata-spine.md) |
| **RSM / HSM** | Recursive State Machine (boxes that call each other) / Hierarchical State Machine (composite states, realized as RSM boxes). | [04](04-automata-spine.md) |
| **`StackAction`** | `Neutral` (Σ_int) / `Push` (Σ_call) / `Pop` (Σ_ret) — the stack action a symbol triggers; fixed by the symbol class (visibility). | [04](04-automata-spine.md) |
| **`MAX_STACK_DEPTH`** | `4096` — the shared static bound (WF + conformance + RLM) that keeps the model finite/decidable. | [04](04-automata-spine.md) |
| **well-nested / Dyck-balanced** | A run whose calls and returns match like balanced brackets. | [04](04-automata-spine.md) · [06](06-conformance-and-the-observer.md) |

### Runtime & conformance

| Term | Definition | Chapter |
|------|-----------|---------|
| **`LocalState`** | A flat integer state index in a compiled machine. | [05](05-data-model-and-compiled-machines.md) |
| **`LocalMachine`** | A role's compiled CFSM/pushdown automaton: states, edges, terminals. | [05](05-data-model-and-compiled-machines.md) |
| **`EdgeKind`** | `Internal` (ordinary comm) / `Call{return_state}` (push) / `Return` (pop). | [05](05-data-model-and-compiled-machines.md) |
| **`Network`** | One `LocalMachine` per role + the directed channel topology. | [05](05-data-model-and-compiled-machines.md) |
| **`RoleConfig`** | A pushdown configuration `(state, stack)` for one role during replay. | [05](05-data-model-and-compiled-machines.md) |
| **`Event` / `Trace`** | One communication `(from, to, label)` / an ordered run of them. | [06](06-conformance-and-the-observer.md) |
| **`check_step`** | The pure, total per-machine legality oracle (Internal edges only). | [06](06-conformance-and-the-observer.md) |
| **ε-closure** | The chase of `Call`(push)/`Return`(pop) boundary edges between input events. | [06](06-conformance-and-the-observer.md) |
| **conformance** | A run conforms iff every event is legal, every machine terminal, every stack empty. | [06](06-conformance-and-the-observer.md) |
| **`lift_transcript`** | Maps a recorded `a2a_pattern_*` transcript into a `Trace`, faithful to what the run did. | [06](06-conformance-and-the-observer.md) |
| **observer-first** | The pattern tools stay intact; a read-only monitor lifts their transcripts; a divergence is a *finding*. | [06](06-conformance-and-the-observer.md) |

### A2A, patterns & recursion

| Term | Definition | Chapter |
|------|-----------|---------|
| **A2A** | Agent-to-Agent — the JSON-RPC + mailbox layer the fleet communicates over. | [07](07-a2a-protocol-and-agent-model.md) |
| **mailbox plane** | Durable async notes (`agent_messages` + receipts); 3 addressing dimensions; `inbox` pull is the reliable floor. | [07](07-a2a-protocol-and-agent-model.md) |
| **task plane** | Synchronous request → typed result (`a2a_tasks`, `TaskState`); the substrate for each `Interaction`'s `Send`/`Recv`. | [07](07-a2a-protocol-and-agent-model.md) |
| **`ProtocolId`** | The enum naming all 8 protocols (5 patterns + WorktreeNegotiation + TapePaging + RecursiveCf). | [08](08-five-patterns-as-protocols.md) |
| **the five patterns** | Sequential, Mixture, Distillation, Deliberation, Recursive (RecursiveMAS collaboration patterns). | [08](08-five-patterns-as-protocols.md) |
| **RecursiveCf** | The genuine pushdown protocol — a 2-role `GlobalType` that calls itself via `GlobalCall`. | [08](08-five-patterns-as-protocols.md) |
| **RLM** | Recursive Language Model: decompose a long query, recurse on snippets, stitch the partials. | [09](09-recursive-language-model.md) |
| **`RlmFrame`** | One recursion frame; `depth_remaining` strictly decreases, `budget_remaining` telescopes. The stack of frames is the pushdown store. | [09](09-recursive-language-model.md) |
| **MessageMedium** | `Text` (black-box-legal) / `Latent` (hidden-state, white-box only). | [01](01-cfsm-mpst-foundations.md) · [03](03-safety-metatheorems.md) |
| **black-box law** | A black-box role may appear only on `Text` edges; a `Latent` edge is a `ProjectionError`. | [03](03-safety-metatheorems.md) |
| **anti-recursion guard** | Fleet leaves are MCP-disabled, so a leaf cannot re-enter `a2a_pattern_*`. | [03](03-safety-metatheorems.md) · [11](11-crucible-plan-execution.md) |

### State, consumer & algebra

| Term | Definition | Chapter |
|------|-----------|---------|
| **"the trace is the position"** | Per-role state = `replay(trace)` through the projection functor; nothing else is checkpointed. | [10](10-state-is-the-trace.md) |
| **session** (`orchestration_sessions`) | A thin resumable row: protocol, bindings, unflushed transcript, `frame_stack`. | [10](10-state-is-the-trace.md) |
| **content plane** (the tape) | What evidence is resident in the window at a position; bit-identical on resume (logical clock). | [10](10-state-is-the-trace.md) |
| **`csm_synthesize_protocol`** | The keystone fold: a plan tree → a typed `GlobalType` (leaf→interaction, interior→`GlobalBox`, loop→`Rec`/`Var`+Choice). | [11](11-crucible-plan-execution.md) |
| **verification is structural** | The Critic-gated loop's only exit runs through `pass`, so the machine cannot terminate unverified. | [11](11-crucible-plan-execution.md) |
| **CT-1 / CT-2 / CT-3** | Projection-as-functor / the `then` monoid + projection homomorphism / the string-diagram tensor. | [12](12-category-theory-layer.md) |
| **`csm_protocol_to_tla`** | The deterministic, faithful-by-construction `GlobalType`→TLA⁺ encoder. | [13](13-formal-verification-artifacts.md) |

---

## 15.2 Notation table

All symbols, always in backticks in prose:

| Symbol | Reads as |
|--------|----------|
| `G`, `L` | a global type; a local type |
| `G ↾ r` | the projection of `G` onto role `r` |
| `from → to : ℓ . G` | `from` sends `ℓ` to `to`, then `G` (`Interaction`) |
| `from → to { ℓᵢ : Gᵢ }` | sender-driven choice (`Choice`) |
| `!to⟨ℓ⟩ . L` | send `ℓ` to `to`, then `L` (`Send`) |
| `?from⟨ℓ⟩ . L` | receive `ℓ` from `from`, then `L` (`Recv`) |
| `⊕to{ ℓᵢ : Lᵢ }` | internal choice / select (projection onto a sender) |
| `&from{ ℓᵢ : Lᵢ }` | external choice / branch (projection onto a receiver, or merged bystander) |
| `μ t. G`, `t` | recursion binder; back-edge |
| `call C[σ] . G` | RSM call to named `C`, roles renamed by `σ`, return, then `G` (`GlobalCall`) |
| `box⟨e⟩{ B }⟨x⟩ . G` | inline box: push on `e`, run `B`, pop on `x`, then `G` (`GlobalBox`) |
| `Σ_int`, `Σ_call`, `Σ_ret` | the internal / call / return alphabet partition of a VPA |
| `⊓` | the merge operation on projected local types |
| `;` | sequential composition (`GlobalType::then`); `End` is its unit |
| `⊗` | the monoidal tensor (independent parallel composition) |
| `⊊`, `⊆` | proper subset; subset (the language hierarchy) |
| `ε` | the empty/silent move (a `Call`/`Return` boundary in conformance replay) |
| `end` | protocol completion (`End`) |

---

## 15.3 Master bibliography

DOIs verified against Crossref; arXiv IDs against the live arXiv.

1. R. Alur, P. Madhusudan. "Visibly pushdown languages." *STOC '04*, 2004. [doi:10.1145/1007352.1007390](https://doi.org/10.1145/1007352.1007390)
2. R. Alur et al. "Analysis of recursive state machines." *ACM TOPLAS*, 27(4), 2005. [doi:10.1145/1075382.1075387](https://doi.org/10.1145/1075382.1075387)
3. D. Harel. "Statecharts: a visual formalism for complex systems." *Sci. Comput. Program.*, 8(3), 1987. [doi:10.1016/0167-6423(87)90035-9](https://doi.org/10.1016/0167-6423(87)90035-9)
4. K. Honda, N. Yoshida, M. Carbone. "Multiparty asynchronous session types." *POPL '08*, 2008. [doi:10.1145/1328438.1328472](https://doi.org/10.1145/1328438.1328472)
5. K. Honda, N. Yoshida, M. Carbone. "Multiparty asynchronous session types." *JACM*, 63(1):9, 2016. [doi:10.1145/2827695](https://doi.org/10.1145/2827695)
6. A. Scalas, N. Yoshida. "Less is more: multiparty session types revisited." *POPL '19*, 2019. [doi:10.1145/3290343](https://doi.org/10.1145/3290343)
7. D. Brand, P. Zafiropulo. "On communicating finite-state machines." *JACM*, 30(2), 1983. [doi:10.1145/322374.322380](https://doi.org/10.1145/322374.322380)
8. P. Thiemann, V. T. Vasconcelos. "Context-free session types." *ICFP '16*, 2016. [doi:10.1145/2951913.2951926](https://doi.org/10.1145/2951913.2951926)
9. A. Das, H. DeYoung, A. Mordido, F. Pfenning. "Nested session types." *ESOP 2021*. [doi:10.1007/978-3-030-72019-3_7](https://doi.org/10.1007/978-3-030-72019-3_7)
10. A. L. Zhang, T. Kraska, O. Khattab. "Recursive Language Models." arXiv:2512.24601, 2025. <https://arxiv.org/abs/2512.24601>
11. Yang et al. "Recursive Multi-Agent Systems." arXiv:2604.25917, 2026.
12. L. Caires, F. Pfenning. "Session types as intuitionistic linear propositions." *CONCUR 2010*. [doi:10.1007/978-3-642-15375-4_16](https://doi.org/10.1007/978-3-642-15375-4_16)
13. P. Wadler. "Propositions as sessions." *ICFP '12*, 2012. [doi:10.1145/2364527.2364568](https://doi.org/10.1145/2364527.2364568)
14. D. Angluin. "Learning regular sets from queries and counterexamples." *Information and Computation*, 75(2), 1987. [doi:10.1016/0890-5401(87)90052-6](https://doi.org/10.1016/0890-5401(87)90052-6)
15. L. Lamport. "Time, clocks, and the ordering of events in a distributed system." *CACM*, 21(7), 1978. [doi:10.1145/359545.359563](https://doi.org/10.1145/359545.359563)

---

*This is the end of the treatise. Back to [README](README.md) · Start over at
[00 — Motivation](00-motivation-and-overview.md).*
