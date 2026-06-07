------------------------ MODULE WorkItemRecordEvidenceBoundary ------------------------
(***************************************************************************)
(* `work_item_record_evidence` request boundary and manual-source trust.    *)
(*                                                                         *)
(* MCP evidence rows are always source='manual'. They may be useful audit   *)
(* notes, but they cannot satisfy the trusted-evidence gate or verify work. *)
(* The wrapper validates local fields before the INSERT ... SELECT lookup   *)
(* and increments the evidence counter only after a row is inserted.        *)
(***************************************************************************)

EXTENDS Naturals

Requests ==
    {"valid",
     "bad_criterion_id",
     "bad_verdict",
     "negative_coverage_count",
     "negative_coverage_total",
     "coverage_exceeds_total",
     "bad_detail_json",
     "detail_too_large",
     "commit_sha_too_large",
     "missing_criterion"}

NoReq == "none"

Reasons ==
    {"none", "bad_criterion_id", "bad_verdict", "negative_coverage_count",
     "negative_coverage_total", "coverage_exceeds_total", "bad_detail_json",
     "detail_too_large", "commit_sha_too_large", "missing_criterion"}

Sources == {"none", "manual"}

Resp(rejected, reason, criterion_lookup, evidence_rows, source, evidence_counter, status) ==
    [ rejected |-> rejected,
      reason |-> reason,
      criterion_lookup |-> criterion_lookup,
      evidence_rows |-> evidence_rows,
      source |-> source,
      evidence_counter |-> evidence_counter,
      status |-> status ]

NoResp == Resp(FALSE, "none", 0, 0, "none", 0, "unchanged")

Reject(reason, criterion_lookup) ==
    Resp(TRUE, reason, criterion_lookup, 0, "none", 0, "unchanged")

Accept ==
    Resp(FALSE, "none", 1, 1, "manual", 1, "unchanged")

Evaluate(r) ==
    CASE r = "valid" -> Accept
      [] r = "bad_criterion_id" -> Reject("bad_criterion_id", 0)
      [] r = "bad_verdict" -> Reject("bad_verdict", 0)
      [] r = "negative_coverage_count" -> Reject("negative_coverage_count", 0)
      [] r = "negative_coverage_total" -> Reject("negative_coverage_total", 0)
      [] r = "coverage_exceeds_total" -> Reject("coverage_exceeds_total", 0)
      [] r = "bad_detail_json" -> Reject("bad_detail_json", 0)
      [] r = "detail_too_large" -> Reject("detail_too_large", 0)
      [] r = "commit_sha_too_large" -> Reject("commit_sha_too_large", 0)
      [] r = "missing_criterion" -> Reject("missing_criterion", 1)
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
    Requests \ {"valid", "missing_criterion"}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.criterion_lookup \in 0..1
    /\ resp.evidence_rows \in 0..1
    /\ resp.source \in Sources
    /\ resp.evidence_counter \in 0..1
    /\ resp.status = "unchanged"

LocalInvalidBeforeLookup ==
    req \in LocalRejects =>
        /\ resp.criterion_lookup = 0
        /\ resp.evidence_rows = 0
        /\ resp.evidence_counter = 0

MissingCriterionWritesNothing ==
    req = "missing_criterion" =>
        /\ resp.criterion_lookup = 1
        /\ resp.evidence_rows = 0
        /\ resp.evidence_counter = 0

RejectedWritesNothing ==
    req # NoReq /\ resp.rejected =>
        /\ resp.evidence_rows = 0
        /\ resp.source = "none"
        /\ resp.evidence_counter = 0
        /\ resp.status = "unchanged"

AcceptedWritesManualEvidenceOnly ==
    req = "valid" =>
        /\ resp.evidence_rows = 1
        /\ resp.source = "manual"
        /\ resp.evidence_counter = 1
        /\ resp.status = "unchanged"

CounterAfterInsert ==
    resp.evidence_counter = 1 =>
        /\ resp.evidence_rows = 1
        /\ req = "valid"

NoTrustedEvidenceFromMcp ==
    req # NoReq /\ resp.evidence_rows = 1 => resp.source = "manual"

NoSelfVerification ==
    req # NoReq => resp.status = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        LocalInvalidBeforeLookup /\
        MissingCriterionWritesNothing /\
        RejectedWritesNothing /\
        AcceptedWritesManualEvidenceOnly /\
        CounterAfterInsert /\
        NoTrustedEvidenceFromMcp /\
        NoSelfVerification)

================================================================================
