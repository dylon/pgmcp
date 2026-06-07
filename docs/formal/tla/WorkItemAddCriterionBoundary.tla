------------------------- MODULE WorkItemAddCriterionBoundary -------------------------
(***************************************************************************)
(* `work_item_add_criterion` request boundary and trust invariant.          *)
(*                                                                         *)
(* MCP callers may add acceptance criteria, but this action is not trusted  *)
(* evidence and cannot verify work. The wrapper validates bounded text and  *)
(* closed vocabularies before the insert; missing work items fail after     *)
(* validation but before any criterion row is written.                      *)
(***************************************************************************)

EXTENDS Naturals

Requests ==
    {"valid",
     "blank_description",
     "description_too_long",
     "bad_kind",
     "bad_coverage",
     "bad_gate",
     "uri_too_long",
     "missing_item"}

NoReq == "none"

Reasons ==
    {"none", "blank_description", "description_too_long", "bad_kind",
     "bad_coverage", "bad_gate", "uri_too_long", "missing_item"}

Resp(rejected, reason, item_lookup, criterion_rows, evidence_rows, status) ==
    [ rejected |-> rejected,
      reason |-> reason,
      item_lookup |-> item_lookup,
      criterion_rows |-> criterion_rows,
      evidence_rows |-> evidence_rows,
      status |-> status ]

NoResp == Resp(FALSE, "none", 0, 0, 0, "unchanged")

Reject(reason, item_lookup) == Resp(TRUE, reason, item_lookup, 0, 0, "unchanged")

Accept == Resp(FALSE, "none", 1, 1, 0, "unchanged")

Evaluate(r) ==
    CASE r = "valid" -> Accept
      [] r = "blank_description" -> Reject("blank_description", 0)
      [] r = "description_too_long" -> Reject("description_too_long", 0)
      [] r = "bad_kind" -> Reject("bad_kind", 0)
      [] r = "bad_coverage" -> Reject("bad_coverage", 0)
      [] r = "bad_gate" -> Reject("bad_gate", 0)
      [] r = "uri_too_long" -> Reject("uri_too_long", 0)
      [] r = "missing_item" -> Reject("missing_item", 1)
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

LocalRejects ==
    {"blank_description", "description_too_long", "bad_kind",
     "bad_coverage", "bad_gate", "uri_too_long"}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.item_lookup \in 0..1
    /\ resp.criterion_rows \in 0..1
    /\ resp.evidence_rows = 0
    /\ resp.status = "unchanged"

LocalInvalidBeforeLookup ==
    req \in LocalRejects =>
        /\ resp.item_lookup = 0
        /\ resp.criterion_rows = 0

MissingItemWritesNothing ==
    req = "missing_item" =>
        /\ resp.item_lookup = 1
        /\ resp.criterion_rows = 0

RejectedWritesNothing ==
    req # NoReq /\ resp.rejected =>
        /\ resp.criterion_rows = 0
        /\ resp.evidence_rows = 0
        /\ resp.status = "unchanged"

AcceptedWritesOneCriterionOnly ==
    req = "valid" =>
        /\ resp.criterion_rows = 1
        /\ resp.evidence_rows = 0
        /\ resp.status = "unchanged"

NoSelfVerification ==
    req # NoReq =>
        /\ resp.evidence_rows = 0
        /\ resp.status = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        LocalInvalidBeforeLookup /\
        MissingItemWritesNothing /\
        RejectedWritesNothing /\
        AcceptedWritesOneCriterionOnly /\
        NoSelfVerification)

================================================================================
