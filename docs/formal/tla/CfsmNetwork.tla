---- MODULE CfsmNetwork ----
(*
 * ADR-009 — CFSM network for the Deliberation coordination protocol:
 *
 *   μ t. O → R : reflect_req .
 *        R → O { converged: O → T : finish  . T → O : final  . end
 *              ; continue : O → T : act_req . T → O : result . t   }
 *
 * modeled as the synchronous (rendezvous) product of the three projected
 * local machines — O(rchestrator), R(eflector), T(ool-Caller) — bounded by
 * MaxRounds. This is the flagship instance: it exercises sender-driven choice
 * (R decides), bounded recursion (the continue loop), and a *bystander* (T),
 * which is exactly the case the Rust projector's external-choice merge handles
 * (src/csm/mpst/project.rs).
 *
 * Correspondence: the control states below are the compiled local machines from
 * `compile(project(deliberation, role))` (src/csm/machine.rs). A golden test
 * (`csm::tests::golden_*`) pins the Rust networks so this hand-aligned spec
 * cannot silently drift.
 *
 * Mechanically checked: TLC (CfsmNetwork.cfg; states/ log). The *unbounded*
 * (∀ MaxRounds) safety/progress guarantees are proven in Rocq
 * (docs/formal/rocq, Phase 5). The THEOREMs below state the TLAPS obligations;
 * `tlapm` is not packaged for this host, so they are discharged by TLC on finite
 * instances plus the Rocq metatheory rather than mechanically here.
 *)
EXTENDS Naturals, FiniteSets

CONSTANT MaxRounds          \* bound on continue-loops (the live `max_rounds` clamp)

VARIABLES
    oc,                     \* Orchestrator control state
    rc,                     \* Reflector control state
    tc,                     \* Tool-Caller control state
    round                   \* completed continue-rounds

vars == <<oc, rc, tc, round>>

OStates == {"o_send_reflect", "o_recv_choice", "o_send_finish", "o_recv_final",
            "o_send_act", "o_recv_result", "o_done"}
RStates == {"r_recv_reflect", "r_choose", "r_done"}
TStates == {"t_recv", "t_send_final", "t_send_result", "t_done"}

TypeOK ==
    /\ oc \in OStates
    /\ rc \in RStates
    /\ tc \in TStates
    /\ round \in 0..MaxRounds

Init ==
    /\ oc = "o_send_reflect"
    /\ rc = "r_recv_reflect"
    /\ tc = "t_recv"
    /\ round = 0

\* O → R : reflect_req  (rendezvous: O sends, R receives).
ReflectReq ==
    /\ oc = "o_send_reflect" /\ rc = "r_recv_reflect"
    /\ oc' = "o_recv_choice" /\ rc' = "r_choose"
    /\ UNCHANGED <<tc, round>>

\* R → O : converged  (R selects the converge branch; O branches to it).
Converge ==
    /\ rc = "r_choose" /\ oc = "o_recv_choice"
    /\ rc' = "r_done" /\ oc' = "o_send_finish"
    /\ UNCHANGED <<tc, round>>

\* R → O : continue  (enabled only under the round bound, forcing eventual
\* convergence at the cap — the live `max_rounds` semantics).
Continue ==
    /\ rc = "r_choose" /\ oc = "o_recv_choice" /\ round < MaxRounds
    /\ rc' = "r_recv_reflect" /\ oc' = "o_send_act"
    /\ UNCHANGED <<tc, round>>

\* O → T : finish  (converge branch — Tool-Caller produces the final answer).
Finish ==
    /\ oc = "o_send_finish" /\ tc = "t_recv"
    /\ oc' = "o_recv_final" /\ tc' = "t_send_final"
    /\ UNCHANGED <<rc, round>>

\* T → O : final.
Final ==
    /\ tc = "t_send_final" /\ oc = "o_recv_final"
    /\ tc' = "t_done" /\ oc' = "o_done"
    /\ UNCHANGED <<rc, round>>

\* O → T : act_req  (continue branch).
ActReq ==
    /\ oc = "o_send_act" /\ tc = "t_recv"
    /\ oc' = "o_recv_result" /\ tc' = "t_send_result"
    /\ UNCHANGED <<rc, round>>

\* T → O : result  (closes the round; all three return to start, round++).
Result ==
    /\ tc = "t_send_result" /\ oc = "o_recv_result"
    /\ tc' = "t_recv" /\ oc' = "o_send_reflect"
    /\ round' = round + 1
    /\ UNCHANGED <<rc>>

Next ==
    \/ ReflectReq \/ Converge \/ Continue
    \/ Finish \/ Final \/ ActReq \/ Result

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

(* === Safety invariants === *)

AllDone == oc = "o_done" /\ rc = "r_done" /\ tc = "t_done"

RoundsBounded == round <= MaxRounds

\* Synchronous discipline: whenever O awaits a reply, its partner is in the
\* matching send/terminal state — no orphaned half-communication.
NoOrphan ==
    /\ (oc = "o_recv_final")  => (tc \in {"t_send_final", "t_done"})
    /\ (oc = "o_recv_result") => (tc \in {"t_send_result", "t_recv"})
    /\ (oc = "o_recv_choice") => (rc \in {"r_choose", "r_done", "r_recv_reflect"})

Invariants ==
    /\ TypeOK
    /\ RoundsBounded
    /\ NoOrphan

(* === Deadlock-freedom & termination === *)

\* In every non-terminal reachable state, some protocol action is enabled.
DeadlockFreedom == (~AllDone) => ENABLED Next

\* The protocol always eventually completes: the round cap forces the Reflector
\* to converge, after which O drives T to finish.
EventualTermination == <>AllDone

(* TLAPS obligations (discharged by TLC finite-model-checking + the Phase-5 Rocq
 * metatheory; `tlapm` not available on this host):
 *
 *   THEOREM Safety == Spec => []Invariants
 *     proof: <1>1 Init => Invariants  (by DEF Init, Invariants, TypeOK)
 *            <1>2 Invariants /\ [Next]_vars => Invariants'
 *                 (case split on the seven actions; each preserves the three
 *                  conjuncts — RoundsBounded since only Result increments round
 *                  and Continue guards round < MaxRounds)
 *            <1> QED by <1>1, <1>2, PTL def Spec
 *
 *   THEOREM Liveness == Spec => EventualTermination
 *     proof: round strictly increases on each Result and is bounded by
 *            MaxRounds, so the continue-loop is well-founded; at round=MaxRounds
 *            only Converge is enabled from r_choose, leading to AllDone; conclude
 *            by WF_vars(Next).
 *)
THEOREM Safety == Spec => []Invariants
THEOREM Liveness == Spec => EventualTermination
====
