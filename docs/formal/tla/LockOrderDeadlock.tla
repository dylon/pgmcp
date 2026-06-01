---------------------------- MODULE LockOrderDeadlock ----------------------------
(***************************************************************************)
(* ADR-011 — operational model behind the shadow-ASR lock-order            *)
(* deadlock analysis (src/graph/lock_order.rs, `tool_deadlock_cycles`).    *)
(*                                                                         *)
(* N processes acquire/release a finite set of locks, each along a fixed   *)
(* acquisition order `AcqOrder[p]` (its inlined sync_ops skeleton). The    *)
(* `Deadlock` predicate is the Coffman circular-wait condition. This spec  *)
(* checks the SOUNDNESS direction the verify.sh gate cares about: when     *)
(* every process's order is consistent with one global ranking `Rank`      *)
(* (i.e. the lock-order graph is ACYCLIC), `[]NoDeadlock` holds — ordered  *)
(* acquisition prevents circular wait (Havender 1968). The unbounded       *)
(* ∀N/∀graph theorem is the Rocq proof (LockOrderDeadlock.v); this TLC run *)
(* is the bounded operational confirmation.                                *)
(*                                                                         *)
(* WITNESS (manual, NOT gated — it is red by design): rerun with an        *)
(* inverted order `p2 :> <<b, a>>` and INVARIANT NoDeadlock; TLC reports    *)
(* the reachable circular-wait state with a trace, exhibiting the cycle    *)
(* `tool_deadlock_cycles` flags. The gate ships only the green safe config.*)
(***************************************************************************)
EXTENDS Naturals, Sequences, TLC

\* The bounded model is concrete (TLC .cfg cannot hold `:>`/`@@` function
\* literals as CONSTANTS): 2 processes, 2 locks (10, 20), both acquiring in the
\* same Rank-increasing order 10-then-20 → the lock-order graph is acyclic.
\* To witness a deadlock, invert process 2 (AcqOrder 2 :> <<20, 10>>) and check
\* INVARIANT NoDeadlock: TLC then reports the reachable circular wait (red by
\* design — a manual check, not part of the green gate).
Procs    == {1, 2}
Locks    == {10, 20}
AcqOrder == (1 :> <<10, 20>> @@ 2 :> <<10, 20>>)
Rank     == (10 :> 1 @@ 20 :> 2)

VARIABLES
    held,       \* held[p] : set of locks p currently holds
    pc          \* pc[p]   : index of p's NEXT acquire (1 .. Len+1)

vars == <<held, pc>>

NextLock(p) == AcqOrder[p][pc[p]]
HasAcq(p)   == pc[p] <= Len(AcqOrder[p])

TypeOK ==
    /\ held \in [Procs -> SUBSET Locks]
    /\ pc \in [Procs -> Nat]
    /\ \A p \in Procs : pc[p] \in 1..(Len(AcqOrder[p]) + 1)

Init ==
    /\ held = [p \in Procs |-> {}]
    /\ pc   = [p \in Procs |-> 1]

\* ACQUIRE: p takes its next lock iff free; otherwise p is (silently) blocked.
Acquire(p) ==
    /\ HasAcq(p)
    /\ LET l == NextLock(p) IN
         /\ \A q \in Procs : l \notin held[q]
         /\ held' = [held EXCEPT ![p] = @ \cup {l}]
         /\ pc'   = [pc   EXCEPT ![p] = @ + 1]

\* RELEASE: once past its last acquire, p drops everything and restarts (keeps
\* the state space finite and lets TLC see steady-state behaviour).
Release(p) ==
    /\ ~HasAcq(p)
    /\ held[p] # {}
    /\ held' = [held EXCEPT ![p] = {}]
    /\ pc'   = [pc   EXCEPT ![p] = 1]

Next == \E p \in Procs : Acquire(p) \/ Release(p)
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* p waits-for q: p still wants a lock that q currently holds.
WaitsFor(p, q) ==
    /\ p # q
    /\ HasAcq(p)
    /\ NextLock(p) \in held[q]

\* Circular wait (Coffman condition #4): two distinct processes each waiting on
\* a lock the other holds. For the bounded model (|Procs| = 2) a length-2 cycle
\* is the complete witness; for larger configs extend to a general orbit.
Deadlock == \E p, q \in Procs : WaitsFor(p, q) /\ WaitsFor(q, p)

NoDeadlock == ~Deadlock

\* Rank-consistency = the extracted lock-order relation is acyclic (a linear
\* Rank is a topological order, which exists iff the graph is a DAG).
RankConsistent ==
    \A p \in Procs :
        \A i, j \in 1..Len(AcqOrder[p]) :
            i < j => Rank[AcqOrder[p][i]] < Rank[AcqOrder[p][j]]

THEOREM SafeIsDeadlockFree == (Spec /\ RankConsistent) => []NoDeadlock
=============================================================================
