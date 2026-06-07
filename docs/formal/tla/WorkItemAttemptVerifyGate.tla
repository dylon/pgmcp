--------------------------- MODULE WorkItemAttemptVerifyGate ---------------------------
(***************************************************************************)
(* `work_item_attempt_verify` gatekeeper transition boundary.              *)
(*                                                                         *)
(* The tool runs the status transition as Actor::Gatekeeper. Missing items, *)
(* non-verifiable statuses, absent evidence, and manual-only evidence must  *)
(* leave status unchanged. Only sufficient trusted passing evidence can     *)
(* publish verified, and the verification counter advances only afterward.  *)
(***************************************************************************)

EXTENDS Naturals

Requests ==
    {"trusted_pass",
     "missing_item",
     "wrong_status",
     "no_evidence",
     "manual_only",
     "trusted_fail"}

NoReq == "none"

Reasons ==
    {"none", "missing_item", "wrong_status", "no_evidence",
     "manual_only", "trusted_fail"}

Statuses == {"unchanged", "verified"}

Resp(rejected, reason, item_lookup, trusted_evidence, status, verification_counter) ==
    [ rejected |-> rejected,
      reason |-> reason,
      item_lookup |-> item_lookup,
      trusted_evidence |-> trusted_evidence,
      status |-> status,
      verification_counter |-> verification_counter ]

NoResp == Resp(FALSE, "none", 0, FALSE, "unchanged", 0)

Reject(reason, item_lookup, trusted_evidence) ==
    Resp(TRUE, reason, item_lookup, trusted_evidence, "unchanged", 0)

Accept ==
    Resp(FALSE, "none", 1, TRUE, "verified", 1)

Evaluate(r) ==
    CASE r = "trusted_pass" -> Accept
      [] r = "missing_item" -> Reject("missing_item", 1, FALSE)
      [] r = "wrong_status" -> Reject("wrong_status", 1, TRUE)
      [] r = "no_evidence" -> Reject("no_evidence", 1, FALSE)
      [] r = "manual_only" -> Reject("manual_only", 1, FALSE)
      [] r = "trusted_fail" -> Reject("trusted_fail", 1, FALSE)
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

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.item_lookup \in 0..1
    /\ resp.trusted_evidence \in BOOLEAN
    /\ resp.status \in Statuses
    /\ resp.verification_counter \in 0..1

RejectedLeavesStatusAndStats ==
    req # NoReq /\ resp.rejected =>
        /\ resp.status = "unchanged"
        /\ resp.verification_counter = 0

ManualEvidenceCannotVerify ==
    req = "manual_only" =>
        /\ resp.status = "unchanged"
        /\ resp.verification_counter = 0

TrustedEvidenceRequired ==
    resp.status = "verified" =>
        /\ resp.trusted_evidence = TRUE
        /\ req = "trusted_pass"

CounterAfterVerifiedTransition ==
    resp.verification_counter = 1 =>
        /\ resp.status = "verified"
        /\ req = "trusted_pass"

OnlyTrustedPassVerifies ==
    req # NoReq /\ req # "trusted_pass" =>
        resp.status = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        RejectedLeavesStatusAndStats /\
        ManualEvidenceCannotVerify /\
        TrustedEvidenceRequired /\
        CounterAfterVerifiedTransition /\
        OnlyTrustedPassVerifies)

================================================================================
