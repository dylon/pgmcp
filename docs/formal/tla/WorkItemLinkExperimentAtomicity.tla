----------------------- MODULE WorkItemLinkExperimentAtomicity -----------------------
(***************************************************************************)
(* `work_item_link_experiment` validation, transaction, and concurrency     *)
(* boundary.                                                               *)
(*                                                                         *)
(* The tool validates caller-local fields, resolves the experiment before   *)
(* opening a write transaction, then commits the optional tracking item,    *)
(* bridge row, and experiment_verdict criterion atomically. For existing   *)
(* work items, it locks the item row with FOR UPDATE before the             *)
(* lookup-then-insert criterion seed, serializing concurrent link calls for *)
(* the same item and preventing duplicate seeded criteria.                  *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

Writes == {"item", "bridge", "criterion"}
LockModes == {"none", "new_row", "update"}
Reasons ==
    {"none", "blank_slug", "slug_too_long", "bad_hypothesis",
     "title_too_long", "missing_experiment", "missing_item",
     "bridge_failure", "criterion_failure"}

Requests ==
    {"auto_ok",
     "existing_ok_no_criterion",
     "existing_ok_has_criterion",
     "blank_slug",
     "slug_too_long",
     "bad_hypothesis",
     "title_too_long",
     "missing_experiment",
     "missing_item",
     "bridge_failure",
     "criterion_failure"}

LocalRejects ==
    {"blank_slug", "slug_too_long", "bad_hypothesis", "title_too_long"}

TxRejects == {"missing_item", "bridge_failure", "criterion_failure"}
ExistingSuccesses == {"existing_ok_no_criterion", "existing_ok_has_criterion"}

NoReq == "none"

Resp(rejected, reason, tx_started, committed, lock_mode, criterion_lookup_after_lock, visible_writes, create_counter_increments) ==
    [ rejected |-> rejected,
      reason |-> reason,
      tx_started |-> tx_started,
      committed |-> committed,
      lock_mode |-> lock_mode,
      criterion_lookup_after_lock |-> criterion_lookup_after_lock,
      visible_writes |-> visible_writes,
      create_counter_increments |-> create_counter_increments ]

NoResp == Resp(FALSE, "none", FALSE, FALSE, "none", FALSE, {}, 0)

Reject(reason, tx_started, lock_mode, criterion_lookup_after_lock) ==
    Resp(TRUE, reason, tx_started, FALSE, lock_mode, criterion_lookup_after_lock, {}, 0)

AcceptAuto ==
    Resp(FALSE, "none", TRUE, TRUE, "new_row", TRUE,
        {"item", "bridge", "criterion"}, 1)

AcceptExistingNoCriterion ==
    Resp(FALSE, "none", TRUE, TRUE, "update", TRUE,
        {"bridge", "criterion"}, 0)

AcceptExistingHasCriterion ==
    Resp(FALSE, "none", TRUE, TRUE, "update", TRUE,
        {"bridge"}, 0)

Evaluate(r) ==
    CASE r = "auto_ok" -> AcceptAuto
      [] r = "existing_ok_no_criterion" -> AcceptExistingNoCriterion
      [] r = "existing_ok_has_criterion" -> AcceptExistingHasCriterion
      [] r = "blank_slug" -> Reject("blank_slug", FALSE, "none", FALSE)
      [] r = "slug_too_long" -> Reject("slug_too_long", FALSE, "none", FALSE)
      [] r = "bad_hypothesis" -> Reject("bad_hypothesis", FALSE, "none", FALSE)
      [] r = "title_too_long" -> Reject("title_too_long", FALSE, "none", FALSE)
      [] r = "missing_experiment" -> Reject("missing_experiment", FALSE, "none", FALSE)
      [] r = "missing_item" -> Reject("missing_item", TRUE, "none", FALSE)
      [] r = "bridge_failure" -> Reject("bridge_failure", TRUE, "new_row", TRUE)
      [] r = "criterion_failure" -> Reject("criterion_failure", TRUE, "new_row", TRUE)
      [] OTHER -> NoResp

(***************************************************************************)
(* Two concurrent valid link calls for the same existing work item.         *)
(***************************************************************************)

Txns == {"link_a", "link_b"}
NoOwner == "none"
Owners == Txns \cup {NoOwner}

VARIABLES req, resp, pc, rowLock, criterionPresent, criterionInserts, done

vars == <<req, resp, pc, rowLock, criterionPresent, criterionInserts, done>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp
    /\ pc = [t \in Txns |-> 1]
    /\ rowLock = NoOwner
    /\ criterionPresent = FALSE
    /\ criterionInserts = [t \in Txns |-> FALSE]
    /\ done = {}

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)
    /\ UNCHANGED <<pc, rowLock, criterionPresent, criterionInserts, done>>

AcquireItemLock(t) ==
    /\ t \in Txns
    /\ pc[t] = 1
    /\ rowLock = NoOwner
    /\ rowLock' = t
    /\ pc' = [pc EXCEPT ![t] = 2]
    /\ UNCHANGED <<req, resp, criterionPresent, criterionInserts, done>>

LookupCriterion(t) ==
    /\ t \in Txns
    /\ pc[t] = 2
    /\ rowLock = t
    /\ pc' = [pc EXCEPT ![t] = 3]
    /\ UNCHANGED <<req, resp, rowLock, criterionPresent, criterionInserts, done>>

InsertCriterion(t) ==
    /\ t \in Txns
    /\ pc[t] = 3
    /\ rowLock = t
    /\ criterionPresent = FALSE
    /\ criterionPresent' = TRUE
    /\ criterionInserts' = [criterionInserts EXCEPT ![t] = TRUE]
    /\ pc' = [pc EXCEPT ![t] = 4]
    /\ UNCHANGED <<req, resp, rowLock, done>>

SkipCriterion(t) ==
    /\ t \in Txns
    /\ pc[t] = 3
    /\ rowLock = t
    /\ criterionPresent = TRUE
    /\ pc' = [pc EXCEPT ![t] = 4]
    /\ UNCHANGED <<req, resp, rowLock, criterionPresent, criterionInserts, done>>

CommitConcurrent(t) ==
    /\ t \in Txns
    /\ pc[t] = 4
    /\ rowLock = t
    /\ rowLock' = NoOwner
    /\ done' = done \cup {t}
    /\ pc' = [pc EXCEPT ![t] = 5]
    /\ UNCHANGED <<req, resp, criterionPresent, criterionInserts>>

DoneStutter ==
    /\ req # NoReq
    /\ done = Txns
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ \E t \in Txns : AcquireItemLock(t)
    \/ \E t \in Txns : LookupCriterion(t)
    \/ \E t \in Txns : InsertCriterion(t)
    \/ \E t \in Txns : SkipCriterion(t)
    \/ \E t \in Txns : CommitConcurrent(t)
    \/ DoneStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.tx_started \in BOOLEAN
    /\ resp.committed \in BOOLEAN
    /\ resp.lock_mode \in LockModes
    /\ resp.criterion_lookup_after_lock \in BOOLEAN
    /\ resp.visible_writes \subseteq Writes
    /\ resp.create_counter_increments \in 0..1
    /\ pc \in [Txns -> 1..5]
    /\ rowLock \in Owners
    /\ criterionPresent \in BOOLEAN
    /\ criterionInserts \in [Txns -> BOOLEAN]
    /\ done \subseteq Txns

RejectedWritesNothing ==
    req # NoReq /\ resp.rejected =>
        /\ resp.visible_writes = {}
        /\ resp.create_counter_increments = 0

LocalValidationBeforeTx ==
    req \in LocalRejects =>
        /\ resp.tx_started = FALSE
        /\ resp.lock_mode = "none"
        /\ resp.visible_writes = {}

ExperimentResolvedBeforeTx ==
    req = "missing_experiment" =>
        /\ resp.tx_started = FALSE
        /\ resp.visible_writes = {}

TransactionFailuresRollback ==
    req \in TxRejects =>
        /\ resp.tx_started = TRUE
        /\ resp.committed = FALSE
        /\ resp.visible_writes = {}

SuccessfulLinksCommitBridge ==
    req \in {"auto_ok"} \cup ExistingSuccesses =>
        /\ resp.committed = TRUE
        /\ "bridge" \in resp.visible_writes

AutoCreateAtomic ==
    req = "auto_ok" =>
        resp.visible_writes = {"item", "bridge", "criterion"}

ExistingLinksNeverCreateItems ==
    req \in ExistingSuccesses =>
        /\ "item" \notin resp.visible_writes
        /\ resp.lock_mode = "update"
        /\ resp.criterion_lookup_after_lock = TRUE

ExistingCriterionSeedIdempotent ==
    req = "existing_ok_has_criterion" =>
        resp.visible_writes = {"bridge"}

CreateCounterAfterCommit ==
    resp.create_counter_increments = 1 =>
        /\ req = "auto_ok"
        /\ resp.committed = TRUE
        /\ "item" \in resp.visible_writes

ExclusiveLockBeforeSeedLookup ==
    req \in ExistingSuccesses =>
        /\ resp.lock_mode = "update"
        /\ resp.criterion_lookup_after_lock = TRUE

RowLockExclusive ==
    rowLock # NoOwner => pc[rowLock] \in 2..4

WaitsFor(t) ==
    IF t \in Txns /\ pc[t] = 1 /\ rowLock # NoOwner
    THEN rowLock
    ELSE NoOwner

NoWaitCycle ==
    ~(\E a, b \in Txns :
        /\ a # b
        /\ WaitsFor(a) = b
        /\ WaitsFor(b) = a)

NoDuplicateConcurrentCriterionSeeds ==
    Cardinality({t \in Txns : criterionInserts[t]}) <= 1

ConcurrentCriterionImpliesPresence ==
    \A t \in Txns : criterionInserts[t] => criterionPresent

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        RejectedWritesNothing /\
        LocalValidationBeforeTx /\
        ExperimentResolvedBeforeTx /\
        TransactionFailuresRollback /\
        SuccessfulLinksCommitBridge /\
        AutoCreateAtomic /\
        ExistingLinksNeverCreateItems /\
        ExistingCriterionSeedIdempotent /\
        CreateCounterAfterCommit /\
        ExclusiveLockBeforeSeedLookup /\
        RowLockExclusive /\
        NoWaitCycle /\
        NoDuplicateConcurrentCriterionSeeds /\
        ConcurrentCriterionImpliesPresence)

================================================================================
