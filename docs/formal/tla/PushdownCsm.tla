---- MODULE PushdownCsm ----
(*
 * ADR-030 (Phase 7) — the PUSHDOWN / hierarchical CSM operational model, with an
 * explicit conformance STACK. The TLC twin of the Rocq `CsmPushdown.v` recognizer
 * (and of src/csm/conformance.rs `replay_to_configs`): a run progresses by Call
 * (push a frame), Ret (pop the matching frame), Int (an ordinary communication —
 * the stack is unchanged), and Finish (the run completes ONLY when the stack is
 * empty — every frame returned, i.e. well-nested).
 *
 * The visibly-pushdown stack action is fixed by the step CLASS (Call/Ret/Int) —
 * Alur & Madhusudan (2004). The push is GUARDED by `MaxStackDepth` (the
 * `DepthExceeded` refusal of the implementation / the `MAX_STACK_DEPTH` bound),
 * which is the linchpin: a finite bound keeps the reachable-state set finite, so
 * (a) TLC has a finite model and (b) the Rocq model is an ordinary `Inductive`
 * (no coinduction) — ADR-030 §9.
 *
 * Mechanically checked: TLC (PushdownCsm.cfg). The runtime bound here is small for
 * finiteness; the real conformance bound is large (MAX_STACK_DEPTH = 4096) and the
 * unbounded ∀ statement is the Rocq theorem `runD_bounded`.
 *)
EXTENDS Naturals, Sequences

CONSTANTS
    MaxStackDepth,          \* the conformance-stack bound (the push guard)
    MaxSteps                \* the call/internal step budget (termination measure)

VARIABLES
    stack,                  \* the pushdown stack (Seq of frame markers)
    steps,                  \* call/internal steps taken, 0 .. MaxSteps
    done                    \* has the run completed (well-nested ⇒ empty stack)?

vars == <<stack, steps, done>>

TypeOK ==
    /\ stack \in Seq({1})           \* frame markers; the pushed value is irrelevant
    /\ Len(stack) <= MaxStackDepth  \* (the implementation pops the top regardless)
    /\ steps \in 0..MaxSteps
    /\ done \in BOOLEAN

Init ==
    /\ stack = <<>>
    /\ steps = 0
    /\ done = FALSE

\* A visibly-pushdown CALL: push a frame, GUARDED by the bound (refuse past it —
\* the DepthExceeded of src/csm/conformance.rs). Consumes one step of the budget.
Call ==
    /\ ~done
    /\ steps < MaxSteps
    /\ Len(stack) < MaxStackDepth
    /\ stack' = Append(stack, 1)
    /\ steps' = steps + 1
    /\ UNCHANGED done

\* A RETURN: pop the matching (top) frame. Draining the stack does not consume the
\* call budget — a run that has stopped calling can always unwind, which is what
\* makes the process deadlock-free and eventually terminating.
Ret ==
    /\ ~done
    /\ Len(stack) > 0
    /\ stack' = SubSeq(stack, 1, Len(stack) - 1)
    /\ UNCHANGED <<steps, done>>

\* An INTERNAL communication: the stack is unchanged (Σ_int).
Int ==
    /\ ~done
    /\ steps < MaxSteps
    /\ stack' = stack
    /\ steps' = steps + 1
    /\ UNCHANGED done

\* The run COMPLETES — only when WELL-NESTED: the stack is empty (every Call was
\* matched by a Ret). This is `check_conformance`'s "terminal ∧ empty stack".
Finish ==
    /\ ~done
    /\ stack = <<>>
    /\ done' = TRUE
    /\ UNCHANGED <<stack, steps>>

Next == Call \/ Ret \/ Int \/ Finish

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

Done == done

\* ── Invariants ───────────────────────────────────────────────────────────────

\* THE LINCHPIN: the stack never exceeds the bound (Rocq `runD_bounded`). Finite
\* reachable configs ⇒ TLC-finite ⇒ Rocq-inductive (ADR-030 §9).
StackBounded == Len(stack) <= MaxStackDepth

\* A completed run is well-nested: it returned every frame (empty stack). The TLC
\* twin of "accepts ⇒ balanced" (Rocq `conformance_sound`).
WellNested == done => stack = <<>>

Invariants ==
    /\ TypeOK
    /\ StackBounded
    /\ WellNested

\* ── Liveness ─────────────────────────────────────────────────────────────────

\* The run is never stuck before completing: it can always Call/Int (budget), Ret
\* (drain), or Finish (empty stack). (`Done` is the legitimate terminal —
\* CHECK_DEADLOCK is off, as in RmasRecursionLoop.cfg.)
DeadlockFreedom == (~Done) => ENABLED Next

\* Every run eventually completes: the call budget is finite and the stack drains,
\* so the lexicographic (MaxSteps − steps, Len(stack)) measure strictly decreases
\* to the terminal — the TLC twin of Rocq `pushdown_terminates`.
EventualTermination == <>Done

THEOREM Safety == Spec => []Invariants
THEOREM Liveness == Spec => EventualTermination
====
