------------------------- MODULE WorkItemDeferReinstateAtomicity -------------------------
(***************************************************************************)
(* `work_item_defer` / `work_item_reinstate` user-authority and atomicity.  *)
(*                                                                         *)
(* These tools require the user token, insert a scope_negotiations audit    *)
(* row, and perform a status transition. The negotiation row and status     *)
(* history must commit together, or neither may be visible.                *)
(***************************************************************************)

EXTENDS Naturals

Requests ==
    {"valid_defer",
     "valid_reinstate",
     "bad_token",
     "blank_reason",
     "missing_item",
     "transition_failure"}

NoReq == "none"

Reasons ==
    {"none", "bad_token", "blank_reason", "missing_item", "transition_failure"}

Statuses == {"unchanged", "deferred", "in_progress"}

Resp(rejected, reason, tx_started, committed, negotiation_rows, status_history_rows, status, status_counter) ==
    [ rejected |-> rejected,
      reason |-> reason,
      tx_started |-> tx_started,
      committed |-> committed,
      negotiation_rows |-> negotiation_rows,
      status_history_rows |-> status_history_rows,
      status |-> status,
      status_counter |-> status_counter ]

NoResp == Resp(FALSE, "none", FALSE, FALSE, 0, 0, "unchanged", 0)

Reject(reason, tx_started) ==
    Resp(TRUE, reason, tx_started, FALSE, 0, 0, "unchanged", 0)

AcceptDefer ==
    Resp(FALSE, "none", TRUE, TRUE, 1, 1, "deferred", 1)

AcceptReinstate ==
    Resp(FALSE, "none", TRUE, TRUE, 1, 1, "in_progress", 1)

Evaluate(r) ==
    CASE r = "valid_defer" -> AcceptDefer
      [] r = "valid_reinstate" -> AcceptReinstate
      [] r = "bad_token" -> Reject("bad_token", FALSE)
      [] r = "blank_reason" -> Reject("blank_reason", FALSE)
      [] r = "missing_item" -> Reject("missing_item", FALSE)
      [] r = "transition_failure" -> Reject("transition_failure", TRUE)
      [] OTHER -> NoResp

VARIABLES req, resp

vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

LocalRejects == {"bad_token", "blank_reason", "missing_item"}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.tx_started \in BOOLEAN
    /\ resp.committed \in BOOLEAN
    /\ resp.negotiation_rows \in 0..1
    /\ resp.status_history_rows \in 0..1
    /\ resp.status \in Statuses
    /\ resp.status_counter \in 0..1

UserTokenRequiredBeforeTx ==
    req = "bad_token" =>
        /\ resp.tx_started = FALSE
        /\ resp.negotiation_rows = 0
        /\ resp.status_history_rows = 0

LocalRejectsWriteNothing ==
    req \in LocalRejects =>
        /\ resp.negotiation_rows = 0
        /\ resp.status_history_rows = 0
        /\ resp.status = "unchanged"
        /\ resp.status_counter = 0

TransitionFailureRollsBackNegotiation ==
    req = "transition_failure" =>
        /\ resp.tx_started = TRUE
        /\ resp.committed = FALSE
        /\ resp.negotiation_rows = 0
        /\ resp.status_history_rows = 0
        /\ resp.status = "unchanged"
        /\ resp.status_counter = 0

SuccessfulDeferAtomic ==
    req = "valid_defer" =>
        /\ resp.committed = TRUE
        /\ resp.negotiation_rows = 1
        /\ resp.status_history_rows = 1
        /\ resp.status = "deferred"

SuccessfulReinstateAtomic ==
    req = "valid_reinstate" =>
        /\ resp.committed = TRUE
        /\ resp.negotiation_rows = 1
        /\ resp.status_history_rows = 1
        /\ resp.status = "in_progress"

CounterAfterCommittedStatusChange ==
    resp.status_counter = 1 =>
        /\ resp.committed = TRUE
        /\ resp.status_history_rows = 1
        /\ resp.status \in {"deferred", "in_progress"}

NoOrphanNegotiation ==
    resp.negotiation_rows = 1 =>
        /\ resp.committed = TRUE
        /\ resp.status_history_rows = 1

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        UserTokenRequiredBeforeTx /\
        LocalRejectsWriteNothing /\
        TransitionFailureRollsBackNegotiation /\
        SuccessfulDeferAtomic /\
        SuccessfulReinstateAtomic /\
        CounterAfterCommittedStatusChange /\
        NoOrphanNegotiation)

================================================================================
