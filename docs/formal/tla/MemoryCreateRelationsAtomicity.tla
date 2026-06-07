------------------------ MODULE MemoryCreateRelationsAtomicity ------------------------
(***************************************************************************)
(* `memory_create_relations` validation, idempotence, and lock-order model. *)
(*                                                                         *)
(* Two valid transactions request the same relation triples in opposite raw *)
(* orders. The implementation normalizes request fields first, then acquires *)
(* transaction-scoped Postgres advisory locks in one sorted total order.    *)
(* Invalid, ambiguous-endpoint, and missing-endpoint requests are modeled   *)
(* as no-write cases.                                                      *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CreateTxns == {"create_ab_bc", "create_bc_ab"}
RejectTxns == {"invalid_blank_field", "ambiguous_endpoint"}
UnresolvedTxns == {"missing_endpoint", "self_loop"}
Txns == CreateTxns \cup RejectTxns \cup UnresolvedTxns

RelationKeys == {"R:A:B:uses", "R:B:C:uses"}
Locks == RelationKeys
NoLock == "none"
Owners == Txns \cup {NoLock}

LockOrder(t) ==
    IF t \in CreateTxns
    THEN <<"R:A:B:uses", "R:B:C:uses">>
    ELSE <<>>

RelationRequest(t) ==
    IF t \in CreateTxns THEN RelationKeys ELSE {}

Rank(l) ==
    CASE l = "R:A:B:uses" -> 1
      [] l = "R:B:C:uses" -> 2
      [] OTHER -> 0

VARIABLES pc, locks, activeRelations, inserted, rejected, unresolved, done

vars == <<pc, locks, activeRelations, inserted, rejected, unresolved, done>>

Init ==
    /\ pc = [t \in Txns |-> 1]
    /\ locks = [l \in Locks |-> NoLock]
    /\ activeRelations = {}
    /\ inserted = [t \in Txns |-> {}]
    /\ rejected = {}
    /\ unresolved = {}
    /\ done = {}

Acquire(t) ==
    /\ t \in CreateTxns
    /\ t \notin done
    /\ pc[t] <= Len(LockOrder(t))
    /\ LET l == LockOrder(t)[pc[t]] IN
       /\ locks[l] \in {NoLock, t}
       /\ locks' = [locks EXCEPT ![l] = t]
    /\ pc' = [pc EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<activeRelations, inserted, rejected, unresolved, done>>

CommitCreate(t) ==
    /\ t \in CreateTxns
    /\ t \notin done
    /\ pc[t] = Len(LockOrder(t)) + 1
    /\ LET newRelations == RelationRequest(t) \ activeRelations IN
       /\ activeRelations' = activeRelations \cup RelationRequest(t)
       /\ inserted' = [inserted EXCEPT ![t] = newRelations]
    /\ locks' = [l \in Locks |-> IF locks[l] = t THEN NoLock ELSE locks[l]]
    /\ done' = done \cup {t}
    /\ UNCHANGED <<pc, rejected, unresolved>>

RejectInvalid(t) ==
    /\ t \in RejectTxns
    /\ t \notin done
    /\ rejected' = rejected \cup {t}
    /\ done' = done \cup {t}
    /\ UNCHANGED <<pc, locks, activeRelations, inserted, unresolved>>

ResolveUnresolved(t) ==
    /\ t \in UnresolvedTxns
    /\ t \notin done
    /\ unresolved' = unresolved \cup {t}
    /\ done' = done \cup {t}
    /\ UNCHANGED <<pc, locks, activeRelations, inserted, rejected>>

DoneStutter ==
    /\ done = Txns
    /\ UNCHANGED vars

Next ==
    \/ \E t \in CreateTxns : Acquire(t)
    \/ \E t \in CreateTxns : CommitCreate(t)
    \/ \E t \in RejectTxns : RejectInvalid(t)
    \/ \E t \in UnresolvedTxns : ResolveUnresolved(t)
    \/ DoneStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ pc \in [Txns -> 1..3]
    /\ locks \in [Locks -> Owners]
    /\ activeRelations \subseteq RelationKeys
    /\ inserted \in [Txns -> SUBSET RelationKeys]
    /\ rejected \subseteq RejectTxns
    /\ unresolved \subseteq UnresolvedTxns
    /\ done \subseteq Txns

WaitingLock(t) ==
    IF t \in done \/ pc[t] > Len(LockOrder(t))
    THEN NoLock
    ELSE LET l == LockOrder(t)[pc[t]] IN
         IF locks[l] \in {NoLock, t} THEN NoLock ELSE l

HeldLocks(t) == {l \in Locks : locks[l] = t}

LockOrderSorted ==
    \A t \in CreateTxns :
        \A i, j \in 1..Len(LockOrder(t)) :
            i < j => Rank(LockOrder(t)[i]) < Rank(LockOrder(t)[j])

NoLockOrderInversion ==
    \A t \in CreateTxns :
        WaitingLock(t) # NoLock =>
            \A l \in HeldLocks(t) : Rank(l) < Rank(WaitingLock(t))

NoDuplicateActiveRelationCreates ==
    \A r \in RelationKeys :
        Cardinality({t \in CreateTxns : r \in inserted[t]}) <= 1

NoInvalidOrUnresolvedWrites ==
    \A t \in RejectTxns \cup UnresolvedTxns : inserted[t] = {}

NoWriteWithoutResolvedEndpoints ==
    \A t \in Txns : inserted[t] # {} => t \in CreateTxns

InsertedRowsAreActive ==
    \A t \in Txns : inserted[t] \subseteq activeRelations

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        LockOrderSorted /\
        NoLockOrderInversion /\
        NoDuplicateActiveRelationCreates /\
        NoInvalidOrUnresolvedWrites /\
        NoWriteWithoutResolvedEndpoints /\
        InsertedRowsAreActive)

================================================================================
