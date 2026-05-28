---- MODULE A2aLinearPipeline ----
(*
 * ADR-009 — the linear (choice-free) coordination patterns, modeled once and
 * instantiated per pattern via the sibling `.cfg` files (NStages = number of
 * orchestrator↔peer request/response pairs):
 *
 *   Sequential   (NStages = 3): O↔Planner, O↔Critic, O↔Solver
 *   Mixture      (NStages = 4): O↔Sp1, O↔Sp2, O↔Sp3, O↔Summarizer
 *   Distillation (NStages = 2): O↔Expert, O↔Learner
 *   Recursive    (NStages = D): O↔Sub1 … O↔Sub_D  (the unrolled RLM self-calls)
 *
 * Each of these is a deterministic line of `2*NStages` communications (a
 * request then a response per stage), so the synchronous product of the
 * projected machines is itself a line — faithfully modeled by the position
 * `step` along it. (The non-linear pattern, Deliberation, is in CfsmNetwork.tla.)
 *
 * Mechanically checked: TLC (per-pattern .cfg; states/ logs). The unbounded
 * (∀ NStages, in particular the recursive ∀ depth termination) guarantee is
 * Rocq T1/T2 (Phase 5).
 *)
EXTENDS Naturals

CONSTANT NStages                \* request/response stages (peers) in the pipeline

VARIABLE step                   \* communications fired so far, 0 .. 2*NStages

vars == <<step>>

TypeOK == step \in 0..(2 * NStages)

Init == step = 0

\* Fire the next scheduled communication (odd step = a request, even = a reply).
Advance ==
    /\ step < 2 * NStages
    /\ step' = step + 1

Next == Advance

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

Done == step = 2 * NStages

StepBounded == step <= 2 * NStages

Invariants == TypeOK /\ StepBounded

\* No deadlock until the pipeline has fully drained.
DeadlockFreedom == (~Done) => ENABLED Next

\* The pipeline always completes (finite, monotone, bounded).
EventualTermination == <>Done

(* TLAPS obligations (TLC + Rocq, as in CfsmNetwork.tla):
 *   THEOREM Spec => []Invariants      (step monotone, bounded by 2*NStages)
 *   THEOREM Spec => EventualTermination (step strictly increases to the bound)
 *)
THEOREM Safety == Spec => []Invariants
THEOREM Liveness == Spec => EventualTermination
====
