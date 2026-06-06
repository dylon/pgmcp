------------------------ MODULE MemoryCreateEntitiesAtomicity ------------------------
(***************************************************************************)
(* `memory_create_entities` atomicity and lock-order model.                *)
(*                                                                         *)
(* Two create transactions request the same identities in opposite raw      *)
(* orders. The implementation normalizes to one total advisory-lock order:  *)
(* entity-identity locks first, then per-entity observation locks, each     *)
(* sorted by key. This model also includes an observation-only transaction  *)
(* to check that create-vs-append lock interaction has no order inversion.  *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Txns == {"create_ab", "create_ba", "append_ba"}
EntityKeys == {"A", "B"}
Contents == {"hello"}
Locks == {"I:A", "I:B", "O:A", "O:B"}
NoLock == "none"
Owners == Txns \cup {NoLock}

LockOrder(t) ==
    IF t = "append_ba"
    THEN <<"O:A", "O:B">>
    ELSE <<"I:A", "I:B", "O:A", "O:B">>

EntityRequest(t) ==
    IF t = "append_ba" THEN {} ELSE EntityKeys

ObservationRecord == [entity: EntityKeys, content: Contents]

ObservationRequest(t) ==
    IF t = "append_ba"
    THEN {}
    ELSE { [entity |-> k, content |-> "hello"] : k \in EntityKeys }

Rank(l) ==
    CASE l = "I:A" -> 1
      [] l = "I:B" -> 2
      [] l = "O:A" -> 3
      [] l = "O:B" -> 4
      [] OTHER -> 0

VARIABLES pc, locks, active, observations, created, obsInserted, done

vars == <<pc, locks, active, observations, created, obsInserted, done>>

Init ==
    /\ pc = [t \in Txns |-> 1]
    /\ locks = [l \in Locks |-> NoLock]
    /\ active = {}
    /\ observations = {}
    /\ created = [t \in Txns |-> {}]
    /\ obsInserted = [t \in Txns |-> {}]
    /\ done = {}

Acquire(t) ==
    /\ t \in Txns
    /\ t \notin done
    /\ pc[t] <= Len(LockOrder(t))
    /\ LET l == LockOrder(t)[pc[t]] IN
       /\ locks[l] \in {NoLock, t}
       /\ locks' = [locks EXCEPT ![l] = t]
    /\ pc' = [pc EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<active, observations, created, obsInserted, done>>

Commit(t) ==
    /\ t \in Txns
    /\ t \notin done
    /\ pc[t] = Len(LockOrder(t)) + 1
    /\ LET newEntities == EntityRequest(t) \ active IN
       LET newObservations == ObservationRequest(t) \ observations IN
       /\ active' = active \cup EntityRequest(t)
       /\ observations' = observations \cup ObservationRequest(t)
       /\ created' = [created EXCEPT ![t] = newEntities]
       /\ obsInserted' = [obsInserted EXCEPT ![t] = newObservations]
    /\ locks' = [l \in Locks |-> IF locks[l] = t THEN NoLock ELSE locks[l]]
    /\ done' = done \cup {t}
    /\ UNCHANGED pc

DoneStutter ==
    /\ done = Txns
    /\ UNCHANGED vars

Next ==
    \/ \E t \in Txns : Acquire(t)
    \/ \E t \in Txns : Commit(t)
    \/ DoneStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ pc \in [Txns -> 1..5]
    /\ locks \in [Locks -> Owners]
    /\ active \subseteq EntityKeys
    /\ observations \subseteq ObservationRecord
    /\ created \in [Txns -> SUBSET EntityKeys]
    /\ obsInserted \in [Txns -> SUBSET ObservationRecord]
    /\ done \subseteq Txns

WaitingLock(t) ==
    IF t \in done \/ pc[t] > Len(LockOrder(t))
    THEN NoLock
    ELSE LET l == LockOrder(t)[pc[t]] IN
         IF locks[l] \in {NoLock, t} THEN NoLock ELSE l

HeldLocks(t) == {l \in Locks : locks[l] = t}

LockOrderSorted ==
    \A t \in Txns :
        \A i, j \in 1..Len(LockOrder(t)) :
            i < j => Rank(LockOrder(t)[i]) < Rank(LockOrder(t)[j])

NoLockOrderInversion ==
    \A t \in Txns :
        WaitingLock(t) # NoLock =>
            \A l \in HeldLocks(t) : Rank(l) < Rank(WaitingLock(t))

NoDuplicateActiveEntityCreates ==
    \A k \in EntityKeys :
        Cardinality({t \in Txns : k \in created[t]}) <= 1

NoDuplicateObservationInserts ==
    \A o \in ObservationRecord :
        Cardinality({t \in Txns : o \in obsInserted[t]}) <= 1

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        LockOrderSorted /\
        NoLockOrderInversion /\
        NoDuplicateActiveEntityCreates /\
        NoDuplicateObservationInserts)

================================================================================
