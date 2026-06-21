---- MODULE VpaConformance ----
(*
 * ADR-030 (Phase 7) — the conformance ACCEPTANCE twin of the Rocq theorem
 * `conformance_sound` / `conformance_correct` (CsmPushdown.v): a visibly-pushdown
 * recognizer reads a run one symbol at a time (Call / Ret / Int), tracking the
 * stack depth and the call/return counts, and ACCEPTS iff it consumed the run
 * with an empty stack and never underflowed. This model-checks the two soundness
 * facts:
 *   - an ACCEPTED run is balanced (equal calls and returns) — `BalancedInv`;
 *   - an UNMATCHED return (a `Ret` at depth 0) makes the run `bad`, so it is
 *     never accepted — `unmatched_return_rejected` (Rocq), here `BadRejectedInv`.
 *
 * Mechanically checked: TLC (VpaConformance.cfg). Bounds are small for a finite
 * model; the unbounded statement is the Rocq theorem.
 *)
EXTENDS Naturals

CONSTANTS
    MaxLen,                 \* longest run the recognizer reads
    MaxDepth                \* the stack-depth bound (visibly-pushdown / MAX_STACK_DEPTH)

VARIABLES
    depth,                  \* current stack depth (#open frames)
    calls,                  \* Call symbols read so far
    rets,                   \* matched Ret symbols read so far
    len,                    \* symbols read so far, 0 .. MaxLen
    bad,                    \* did an unmatched return (Ret at depth 0) occur?
    finished                \* has the recognizer stopped reading?

vars == <<depth, calls, rets, len, bad, finished>>

TypeOK ==
    /\ depth \in 0..MaxDepth
    /\ calls \in 0..MaxLen
    /\ rets \in 0..MaxLen
    /\ len \in 0..MaxLen
    /\ bad \in BOOLEAN
    /\ finished \in BOOLEAN

Init ==
    /\ depth = 0 /\ calls = 0 /\ rets = 0 /\ len = 0
    /\ bad = FALSE /\ finished = FALSE

\* Read a CALL: push (depth+1), guarded by the bound; count it.
ReadCall ==
    /\ ~finished
    /\ len < MaxLen
    /\ depth < MaxDepth
    /\ depth' = depth + 1
    /\ calls' = calls + 1
    /\ len' = len + 1
    /\ UNCHANGED <<rets, bad, finished>>

\* Read a matched RET: pop (depth-1); count it.
ReadRet ==
    /\ ~finished
    /\ len < MaxLen
    /\ depth > 0
    /\ depth' = depth - 1
    /\ rets' = rets + 1
    /\ len' = len + 1
    /\ UNCHANGED <<calls, bad, finished>>

\* Read an UNMATCHED RET (a return at depth 0 — underflow): flag `bad`; the depth
\* stays 0 (there was nothing to pop). This run can never be accepted.
ReadRetBad ==
    /\ ~finished
    /\ len < MaxLen
    /\ depth = 0
    /\ bad' = TRUE
    /\ len' = len + 1
    /\ UNCHANGED <<depth, calls, rets, finished>>

\* Read an INTERNAL symbol: the stack is unchanged.
ReadInt ==
    /\ ~finished
    /\ len < MaxLen
    /\ len' = len + 1
    /\ UNCHANGED <<depth, calls, rets, bad, finished>>

\* Stop reading (the run ended).
Stop ==
    /\ ~finished
    /\ finished' = TRUE
    /\ UNCHANGED <<depth, calls, rets, len, bad>>

Next == ReadCall \/ ReadRet \/ ReadRetBad \/ ReadInt \/ Stop

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* The recognizer ACCEPTS: it finished, never underflowed, and the stack is empty.
accepted == finished /\ ~bad /\ depth = 0

\* ── Invariants ───────────────────────────────────────────────────────────────

\* depth is always #calls − #matched-rets (a matched Ret pops; an unmatched Ret
\* does not). This is the structural balance the recognizer maintains.
DepthInv == depth = calls - rets

\* SOUNDNESS twin: an accepted run is balanced (equal calls and returns).
BalancedInv == accepted => (calls = rets)

\* The stack never exceeds the bound (the linchpin, mirrored from PushdownCsm).
DepthBounded == depth <= MaxDepth

\* An unmatched return is never accepted (Rocq `unmatched_return_rejected`).
BadRejectedInv == bad => ~accepted

Invariants ==
    /\ TypeOK
    /\ DepthInv
    /\ BalancedInv
    /\ DepthBounded
    /\ BadRejectedInv

\* ── Liveness ─────────────────────────────────────────────────────────────────

\* Never stuck before stopping: `Stop` is always available until finished.
DeadlockFreedom == (~finished) => ENABLED Next

\* Every run eventually stops (the length budget is finite).
EventualTermination == <>finished

THEOREM Safety == Spec => []Invariants
THEOREM Liveness == Spec => EventualTermination
====
